// Required for USDT
#![feature(asm)]

extern crate pico_args;
extern crate propolis;
extern crate serde;
extern crate serde_derive;
extern crate toml;

use std::collections::HashMap;
use std::fs::File;
use std::io::{Error, ErrorKind, Result};
use std::path::Path;
use std::sync::Arc;

use propolis::chardev::{Sink, Source};
use propolis::dispatch::DispCtx;
use propolis::hw::chipset::Chipset;
use propolis::hw::ibmpc;
use propolis::hw::ps2ctrl::PS2Ctrl;
use propolis::hw::uart::LpcUart;
use propolis::instance::{Instance, State};
use propolis::vmm::{Builder, Prot};
use propolis::*;

use propolis::usdt::register_probes;

mod config;

const PAGE_OFFSET: u64 = 0xfff;
// Arbitrary ROM limit for now
const MAX_ROM_SIZE: usize = 0x20_0000;

fn parse_args() -> config::Config {
    let args = pico_args::Arguments::from_env();
    if let Some(cpath) = args.free().ok().map(|mut f| f.pop()).flatten() {
        config::parse(&cpath)
    } else {
        eprintln!("usage: propolis <CONFIG.toml>");
        std::process::exit(libc::EXIT_FAILURE);
    }
}

fn build_instance(
    name: &str,
    max_cpu: u8,
    lowmem: usize,
    highmem: usize,
) -> Result<Arc<Instance>> {
    let mut builder = Builder::new(name, true)?
        .max_cpus(max_cpu)?
        .add_mem_region(0, lowmem, Prot::ALL, "lowmem")?
        .add_rom_region(
            0x1_0000_0000 - MAX_ROM_SIZE,
            MAX_ROM_SIZE,
            Prot::READ | Prot::EXEC,
            "bootrom",
        )?
        .add_mmio_region(0xc0000000_usize, 0x20000000_usize, "dev32")?
        .add_mmio_region(0xe0000000_usize, 0x10000000_usize, "pcicfg")?
        .add_mmio_region(
            vmm::MAX_SYSMEM,
            vmm::MAX_PHYSMEM - vmm::MAX_SYSMEM,
            "dev64",
        )?;
    if highmem > 0 {
        builder = builder.add_mem_region(
            0x1_0000_0000,
            highmem,
            Prot::ALL,
            "highmem",
        )?;
    }
    let inst = Instance::create(builder, propolis::vcpu_run_loop)?;
    Ok(inst)
}

fn open_bootrom(path: &str) -> Result<(File, usize)> {
    let fp = File::open(path)?;
    let len = fp.metadata()?.len();
    if len & PAGE_OFFSET != 0 {
        Err(Error::new(
            ErrorKind::InvalidData,
            format!(
                "rom {} length {:x} not aligned to {:x}",
                path,
                len,
                PAGE_OFFSET + 1
            ),
        ))
    } else {
        Ok((fp, len as usize))
    }
}

fn main() {
    // Ensure proper setup of USDT probes
    register_probes().unwrap();

    let config = parse_args();

    let vm_name = config.get_name();
    let cpus = config.get_cpus();

    const GB: usize = 1024 * 1024 * 1024;
    const MB: usize = 1024 * 1024;
    let memsize: usize = config.get_mem() * MB;
    let lowmem = memsize.min(3 * GB);
    let highmem = memsize.saturating_sub(3 * GB);

    let inst = build_instance(vm_name, cpus, lowmem, highmem).unwrap();
    println!("vm {} created", vm_name);

    let (romfp, rom_len) = open_bootrom(config.get_bootrom())
        .unwrap_or_else(|e| panic!("Cannot open bootrom: {}", e));
    let com1_sock = chardev::UDSock::bind(Path::new("./ttya"))
        .unwrap_or_else(|e| panic!("Cannot bind UDSock: {}", e));

    inst.initialize(|machine, mctx, disp, inv| {
        machine.populate_rom("bootrom", |mapping| {
            let mapping = mapping.as_ref();
            if mapping.len() < rom_len {
                return Err(Error::new(ErrorKind::InvalidData, "rom too long"));
            }
            let offset = mapping.len() - rom_len;
            let submapping = mapping.subregion(offset, rom_len).unwrap();
            let nread = submapping.pread(&romfp, rom_len, 0)?;
            if nread != rom_len {
                // TODO: Handle short read
                return Err(Error::new(ErrorKind::InvalidData, "short read"));
            }
            Ok(())
        })?;
        machine.initialize_rtc(lowmem, highmem).unwrap();

        let hdl = machine.get_hdl();
        let chipset = hw::chipset::i440fx::I440Fx::create(Arc::clone(&hdl));
        chipset.attach(mctx);
        let chipset_id = inv
            .register_root(chipset.clone(), "chipset".to_string())
            .map_err(|e| -> std::io::Error { e.into() })?;

        // UARTs
        let com1 = LpcUart::new(chipset.irq_pin(ibmpc::IRQ_COM1).unwrap());
        let com2 = LpcUart::new(chipset.irq_pin(ibmpc::IRQ_COM2).unwrap());
        let com3 = LpcUart::new(chipset.irq_pin(ibmpc::IRQ_COM3).unwrap());
        let com4 = LpcUart::new(chipset.irq_pin(ibmpc::IRQ_COM4).unwrap());

        let ctx = disp.ctx();
        com1_sock.listen(&ctx);
        com1_sock.attach_sink(Arc::clone(&com1) as Arc<dyn Sink<DispCtx>>);
        com1_sock.attach_source(Arc::clone(&com1) as Arc<dyn Source<DispCtx>>);
        com1.source_set_autodiscard(false);

        // XXX: plumb up com2-4, but until then, just auto-discard
        com2.source_set_autodiscard(true);
        com3.source_set_autodiscard(true);
        com4.source_set_autodiscard(true);

        let pio = mctx.pio();
        LpcUart::attach(&com1, pio, ibmpc::PORT_COM1);
        LpcUart::attach(&com2, pio, ibmpc::PORT_COM2);
        LpcUart::attach(&com3, pio, ibmpc::PORT_COM3);
        LpcUart::attach(&com4, pio, ibmpc::PORT_COM4);
        inv.register(chipset_id, com1, "com1".to_string())
            .map_err(|e| -> std::io::Error { e.into() })?;
        inv.register(chipset_id, com2, "com2".to_string())
            .map_err(|e| -> std::io::Error { e.into() })?;
        inv.register(chipset_id, com3, "com3".to_string())
            .map_err(|e| -> std::io::Error { e.into() })?;
        inv.register(chipset_id, com4, "com4".to_string())
            .map_err(|e| -> std::io::Error { e.into() })?;

        // PS/2
        let ps2_ctrl = PS2Ctrl::create();
        ps2_ctrl.attach(pio, chipset.as_ref());
        inv.register(chipset_id, ps2_ctrl, "ps2_ctrl".to_string())
            .map_err(|e| -> std::io::Error { e.into() })?;

        let debug = std::fs::File::create("debug.out").unwrap();
        let buffered = std::io::LineWriter::new(debug);
        let dbg = hw::qemu::debug::QemuDebugPort::create(
            Some(Box::new(buffered) as Box<dyn std::io::Write + Send>),
            pio,
        );
        inv.register(chipset_id, dbg, "debug".to_string())
            .map_err(|e| -> std::io::Error { e.into() })?;

        let mut devices = HashMap::new();

        for (name, dev) in config.devs() {
            let driver = &dev.driver as &str;
            let bdf = if driver.starts_with("pci-") {
                config::parse_bdf(
                    dev.options.get("pci-path").unwrap().as_str().unwrap(),
                )
            } else {
                None
            };
            match driver {
                "pci-virtio-block" => {
                    let disk_path =
                        dev.options.get("disk").unwrap().as_str().unwrap();

                    let readonly: bool = || -> Option<bool> {
                        dev.options.get("readonly")?.as_str()?.parse().ok()
                    }()
                    .unwrap_or(false);

                    let plain =
                        block::FileBdev::create(disk_path, readonly).unwrap();

                    let vioblk = hw::virtio::VirtioBlock::create(
                        0x100,
                        Arc::clone(&plain)
                            as Arc<
                                dyn block::BlockDev<hw::virtio::block::Request>,
                            >,
                    );
                    chipset.pci_attach(bdf.unwrap(), vioblk);

                    plain.start_dispatch(format!("bdev-{} thread", name), disp);
                }
                "pci-virtio-viona" => {
                    let vnic_name =
                        dev.options.get("vnic").unwrap().as_str().unwrap();

                    let viona = hw::virtio::viona::VirtioViona::create(
                        vnic_name, 0x100, &hdl,
                    )
                    .unwrap();
                    chipset.pci_attach(bdf.unwrap(), viona);
                }
                "pci-nvme" => {
                    let nvme = hw::nvme::PciNvme::create(0x1de, 0x1000);
                    devices.insert(&**name, nvme.clone());
                    chipset.pci_attach(bdf.unwrap(), nvme);
                }
                "nvme-ns" => {
                    let disk_path =
                        dev.options.get("disk").unwrap().as_str().unwrap();
                    let nvme_ctrl = dev
                        .options
                        .get("controller")
                        .unwrap()
                        .as_str()
                        .unwrap();

                    let nvme = devices.get(nvme_ctrl).unwrap_or_else(|| {
                        panic!("no such nvme controller: {}", nvme_ctrl)
                    });

                    let readonly: bool = || -> Option<bool> {
                        dev.options.get("readonly")?.as_str()?.parse().ok()
                    }()
                    .unwrap_or(false);

                    let plain =
                        block::FileBdev::create(disk_path, readonly).unwrap();
                    let ns = hw::nvme::NvmeNs::create(plain.clone());

                    if let Err(e) =
                        nvme.with_inner(|nvme: Arc<hw::nvme::PciNvme>| {
                            nvme.add_ns(ns)
                        })
                    {
                        eprintln!("failed to attach nvme-ns: {}", e);
                        std::process::exit(libc::EXIT_FAILURE);
                    }
                    plain.start_dispatch(format!("bdev-{} thread", name), disp);
                }
                _ => {
                    eprintln!("unrecognized driver: {}", name);
                    std::process::exit(libc::EXIT_FAILURE);
                }
            }
        }

        // with all pci devices attached, place their BARs and wire up access to PCI
        // configuration space
        chipset.pci_finalize(&ctx);

        let mut fwcfg = hw::qemu::fwcfg::FwCfgBuilder::new();
        fwcfg
            .add_legacy(
                hw::qemu::fwcfg::LegacyId::SmpCpuCount,
                hw::qemu::fwcfg::FixedItem::new_u32(cpus as u32),
            )
            .unwrap();

        let ramfb = hw::qemu::ramfb::RamFb::create();
        ramfb.attach(&mut fwcfg);

        let fwcfg_dev = fwcfg.finalize();
        fwcfg_dev.attach(pio);

        inv.register(chipset_id, fwcfg_dev, "fwcfg".to_string())
            .map_err(|e| -> std::io::Error { e.into() })?;
        inv.register(chipset_id, ramfb, "ramfb".to_string())
            .map_err(|e| -> std::io::Error { e.into() })?;

        let ncpu = mctx.max_cpus();
        for id in 0..ncpu {
            let mut vcpu = machine.vcpu(id);
            vcpu.set_default_capabs().unwrap();
            vcpu.reboot_state().unwrap();
            vcpu.activate().unwrap();
            // Set BSP to start up
            if id == 0 {
                vcpu.set_run_state(bhyve_api::VRS_RUN).unwrap();
                vcpu.set_reg(bhyve_api::vm_reg_name::VM_REG_GUEST_RIP, 0xfff0)
                    .unwrap();
            }
        }

        Ok(())
    })
    .unwrap_or_else(|e| panic!("Failed to initialize instance: {}", e));

    drop(romfp);

    inst.print();

    // Wait until someone connects to ttya
    println!("Waiting for a connection to ttya...");
    com1_sock.wait_for_connect();

    inst.on_transition(Box::new(|next_state| {
        println!("state cb: {:?}", next_state);
    }));
    inst.set_target_state(State::Run).unwrap();

    inst.wait_for_state(State::Destroy);
    drop(inst);
}
