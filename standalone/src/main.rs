// Required for USDT
#![feature(asm)]

extern crate pico_args;
extern crate propolis;
extern crate serde;
extern crate serde_derive;
extern crate toml;

use std::fs::File;
use std::io::{Error, ErrorKind, Result};
use std::path::Path;
use std::sync::Arc;

use propolis::chardev::{BlockingSource, Sink, Source};
use propolis::hw::chipset::Chipset;
use propolis::hw::ibmpc;
use propolis::hw::ps2ctrl::PS2Ctrl;
use propolis::hw::uart::LpcUart;
use propolis::instance::{Instance, ReqState, State};
use propolis::vmm::{Builder, Prot};
use propolis::*;

use propolis::usdt::register_probes;

use slog::{o, Drain};

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
    log: slog::Logger,
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
    Instance::create(builder.finalize()?, None, Some(log))
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

fn build_log() -> slog::Logger {
    let decorator = slog_term::TermDecorator::new().build();
    let drain = slog_term::CompactFormat::new(decorator).build().fuse();
    let drain = slog_async::Async::new(drain).build().fuse();

    slog::Logger::root(drain, o!())
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

    let log = build_log();
    let inst =
        build_instance(vm_name, cpus, lowmem, highmem, log.clone()).unwrap();
    slog::info!(log, "VM created"; "name" => vm_name);

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
        let chipset = hw::chipset::i440fx::I440Fx::create(machine);
        let chipset_id = inv
            .register(&chipset, "chipset".to_string(), None)
            .map_err(|e| -> std::io::Error { e.into() })?;

        // UARTs
        let com1 = LpcUart::new(chipset.irq_pin(ibmpc::IRQ_COM1).unwrap());
        let com2 = LpcUart::new(chipset.irq_pin(ibmpc::IRQ_COM2).unwrap());
        let com3 = LpcUart::new(chipset.irq_pin(ibmpc::IRQ_COM3).unwrap());
        let com4 = LpcUart::new(chipset.irq_pin(ibmpc::IRQ_COM4).unwrap());

        com1_sock.spawn(
            Arc::clone(&com1) as Arc<dyn Sink>,
            Arc::clone(&com1) as Arc<dyn Source>,
            disp,
        );
        com1.set_autodiscard(false);

        // XXX: plumb up com2-4, but until then, just auto-discard
        com2.set_autodiscard(true);
        com3.set_autodiscard(true);
        com4.set_autodiscard(true);

        let pio = mctx.pio();
        LpcUart::attach(&com1, pio, ibmpc::PORT_COM1);
        LpcUart::attach(&com2, pio, ibmpc::PORT_COM2);
        LpcUart::attach(&com3, pio, ibmpc::PORT_COM3);
        LpcUart::attach(&com4, pio, ibmpc::PORT_COM4);
        inv.register(&com1, "com1".to_string(), Some(chipset_id))
            .map_err(|e| -> std::io::Error { e.into() })?;
        inv.register(&com2, "com2".to_string(), Some(chipset_id))
            .map_err(|e| -> std::io::Error { e.into() })?;
        inv.register(&com3, "com3".to_string(), Some(chipset_id))
            .map_err(|e| -> std::io::Error { e.into() })?;
        inv.register(&com4, "com4".to_string(), Some(chipset_id))
            .map_err(|e| -> std::io::Error { e.into() })?;

        // PS/2
        let ps2_ctrl = PS2Ctrl::create();
        ps2_ctrl.attach(pio, chipset.as_ref());
        inv.register(&ps2_ctrl, "ps2_ctrl".to_string(), Some(chipset_id))
            .map_err(|e| -> std::io::Error { e.into() })?;

        let debug_file = std::fs::File::create("debug.out").unwrap();
        let debug_out = chardev::BlockingFileOutput::new(debug_file).unwrap();
        let debug_device = hw::qemu::debug::QemuDebugPort::create(pio);
        debug_out
            .attach(Arc::clone(&debug_device) as Arc<dyn BlockingSource>, disp);
        inv.register(&debug_device, "debug".to_string(), None)
            .map_err(|e| -> std::io::Error { e.into() })?;

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
                    let block_dev =
                        dev.options.get("block_dev").unwrap().as_str().unwrap();

                    let (backend, creg) = config.block_dev(block_dev);

                    let info = backend.info();
                    let vioblk = hw::virtio::PciVirtioBlock::new(0x100, info);
                    let id = inv
                        .register(&vioblk, format!("vioblk-{}", name), None)
                        .map_err(|e| -> std::io::Error { e.into() })?;
                    let _be_id = inv
                        .register_child(creg, id)
                        .map_err(|e| -> std::io::Error { e.into() })?;

                    backend
                        .attach(vioblk.clone() as Arc<dyn block::Device>, disp);

                    chipset.pci_attach(bdf.unwrap(), vioblk);
                }
                "pci-virtio-viona" => {
                    let vnic_name =
                        dev.options.get("vnic").unwrap().as_str().unwrap();

                    let viona =
                        hw::virtio::PciVirtioViona::new(vnic_name, 0x100, &hdl)
                            .unwrap();
                    inv.register(&viona, format!("viona-{}", name), None)
                        .map_err(|e| -> std::io::Error { e.into() })?;
                    chipset.pci_attach(bdf.unwrap(), viona);
                }
                "pci-nvme" => {
                    let block_dev =
                        dev.options.get("block_dev").unwrap().as_str().unwrap();

                    let (backend, creg) = config.block_dev(block_dev);

                    let info = backend.info();
                    let nvme = hw::nvme::PciNvme::create(0x1de, 0x1000, info);

                    let id =
                        inv.register(&nvme, format!("nvme-{}", name), None)?;
                    let _be_id = inv.register_child(creg, id)?;

                    backend.attach(nvme.clone(), disp);

                    chipset.pci_attach(bdf.unwrap(), nvme);
                }
                _ => {
                    slog::error!(log, "unrecognized driver"; "name" => name);
                    std::process::exit(libc::EXIT_FAILURE);
                }
            }
        }

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

        inv.register(&fwcfg_dev, "fwcfg".to_string(), Some(chipset_id))
            .map_err(|e| -> std::io::Error { e.into() })?;
        inv.register(&ramfb, "ramfb".to_string(), Some(chipset_id))
            .map_err(|e| -> std::io::Error { e.into() })?;

        for mut vcpu in mctx.vcpus() {
            vcpu.set_default_capabs().unwrap();
        }

        Ok(())
    })
    .unwrap_or_else(|e| panic!("Failed to initialize instance: {}", e));

    inst.spawn_vcpu_workers(propolis::vcpu_run_loop)
        .unwrap_or_else(|e| panic!("Failed spawn vCPU workers: {}", e));

    drop(romfp);

    inst.print();

    // Wait until someone connects to ttya
    slog::error!(log, "Waiting for a connection to ttya");
    com1_sock.wait_for_connect();

    inst.on_transition(Box::new(|next_state, ctx| {
        match next_state {
            State::Boot => {
                for mut vcpu in ctx.mctx.vcpus() {
                    vcpu.reboot_state().unwrap();
                    vcpu.activate().unwrap();
                    // Set BSP to start up
                    if vcpu.is_bsp() {
                        vcpu.set_run_state(bhyve_api::VRS_RUN).unwrap();
                        vcpu.set_reg(
                            bhyve_api::vm_reg_name::VM_REG_GUEST_RIP,
                            0xfff0,
                        )
                        .unwrap();
                    }
                }
            }
            _ => {}
        }
    }));
    inst.set_target_state(ReqState::Run).unwrap();

    inst.wait_for_state(State::Destroy);
    drop(inst);
}
