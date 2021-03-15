extern crate pico_args;
extern crate propolis;
extern crate serde;
extern crate serde_derive;
extern crate toml;

use std::fs::File;
use std::io::{Error, ErrorKind, Read, Result};
use std::path::Path;
use std::sync::Arc;

use propolis::chardev::{Sink, Source};
use propolis::hw::chipset::Chipset;
use propolis::hw::ibmpc;
use propolis::hw::ps2ctrl::PS2Ctrl;
use propolis::hw::uart::LpcUart;
use propolis::instance::{Instance, State};
use propolis::vmm::{Builder, Prot};
use propolis::*;

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
) -> Result<Arc<Instance>> {
    let builder = Builder::new(name, true)?
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
    let config = parse_args();

    let vm_name = config.get_name();
    let lowmem: usize = config.get_mem() * 1024 * 1024;
    let cpus = config.get_cpus();

    let inst = build_instance(vm_name, cpus, lowmem).unwrap();
    println!("vm {} created", vm_name);

    let (mut romfp, rom_len) = open_bootrom(config.get_bootrom())
        .unwrap_or_else(|e| panic!("Cannot open bootrom: {}", e));
    let com1_sock = chardev::UDSock::bind(Path::new("./ttya"))
        .unwrap_or_else(|e| panic!("Cannot bind UDSock: {}", e));

    inst.initialize(|machine, mctx, disp, inv| {
        machine.populate_rom("bootrom", |ptr, region_len| {
            if region_len < rom_len {
                return Err(Error::new(ErrorKind::InvalidData, "rom too long"));
            }
            let offset = region_len - rom_len;
            unsafe {
                let write_ptr = ptr.as_ptr().add(offset);
                // TODO: from_raw_parts_mut requires that the data must be
                // properly aligned - is there anything which guarantees
                // alignment about this access?
                let buf = std::slice::from_raw_parts_mut(write_ptr, rom_len);
                match romfp.read(buf) {
                    Ok(n) if n == rom_len => Ok(()),
                    Ok(_) => {
                        // TODO: handle short read
                        Ok(())
                    }
                    Err(e) => Err(e),
                }
            }
        })?;
        machine.initialize_rtc(lowmem).unwrap();

        let hdl = machine.get_hdl();
        let chipset = hw::chipset::i440fx::I440Fx::create(Arc::clone(&hdl));
        chipset.attach(mctx);

        // UARTs
        let com1 = LpcUart::new(chipset.irq_pin(ibmpc::IRQ_COM1).unwrap());
        let com2 = LpcUart::new(chipset.irq_pin(ibmpc::IRQ_COM2).unwrap());
        let com3 = LpcUart::new(chipset.irq_pin(ibmpc::IRQ_COM3).unwrap());
        let com4 = LpcUart::new(chipset.irq_pin(ibmpc::IRQ_COM4).unwrap());

        disp.with_ctx(|ctx| {
            com1_sock.listen(ctx);
        });
        com1_sock.attach_sink(Arc::clone(&com1) as Arc<dyn Sink>);
        com1_sock.attach_source(Arc::clone(&com1) as Arc<dyn Source>);
        com1.source_set_autodiscard(false);

        // XXX: plumb up com2-4, but until then, just auto-discard
        com2.source_set_autodiscard(true);
        com3.source_set_autodiscard(true);
        com4.source_set_autodiscard(true);

        mctx.with_pio(|pio| {
            LpcUart::attach(&com1, pio, ibmpc::PORT_COM1);
            LpcUart::attach(&com2, pio, ibmpc::PORT_COM2);
            LpcUart::attach(&com3, pio, ibmpc::PORT_COM3);
            LpcUart::attach(&com4, pio, ibmpc::PORT_COM4);
        });
        inv.register(com1, "com1".to_string());
        inv.register(com2, "com2".to_string());
        inv.register(com3, "com3".to_string());
        inv.register(com4, "com4".to_string());

        // PS/2
        let ps2_ctrl = PS2Ctrl::create();
        mctx.with_pio(|pio| {
            ps2_ctrl.attach(pio, chipset.as_ref());
        });
        inv.register(ps2_ctrl, "ps2_ctrl".to_string());

        let dbg = mctx.with_pio(|pio| {
            let debug = std::fs::File::create("debug.out").unwrap();
            let buffered = std::io::LineWriter::new(debug);
            hw::qemu::debug::QemuDebugPort::create(
                Some(Box::new(buffered) as Box<dyn std::io::Write + Send>),
                pio,
            )
        });
        inv.register(dbg, "debug".to_string());

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

                    let plain: Arc<
                        block::PlainBdev<hw::virtio::block::Request>,
                    > = block::PlainBdev::create(disk_path).unwrap();

                    let vioblk = hw::virtio::VirtioBlock::create(
                        0x100,
                        Arc::clone(&plain)
                            as Arc<
                                dyn block::BlockDev<hw::virtio::block::Request>,
                            >,
                    );
                    chipset.pci_attach(bdf.unwrap(), vioblk);

                    plain
                        .start_dispatch(format!("bdev-{} thread", name), &disp);
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
                _ => {
                    eprintln!("unrecognized driver: {}", name);
                    std::process::exit(libc::EXIT_FAILURE);
                }
            }
        }

        // with all pci devices attached, place their BARs and wire up access to PCI
        // configuration space
        disp.with_ctx(|ctx| chipset.pci_finalize(ctx));
        inv.register(chipset, "chipset".to_string());

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

        mctx.with_pio(|pio| fwcfg_dev.attach(pio));

        inv.register(fwcfg_dev, "fwcfg".to_string());
        inv.register(ramfb, "ramfb".to_string());

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
