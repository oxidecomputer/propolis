// Required for USDT
#![cfg_attr(
    all(feature = "dtrace-probes", target_os = "macos"),
    feature(asm_sym)
)]

use std::fs::File;
use std::io::{Error, ErrorKind, Result};
use std::path::Path;
use std::sync::{Arc, Once};
use std::time::SystemTime;

use anyhow::Context;
use clap::Parser;
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
use tokio::runtime::Handle;

mod config;
mod snapshot;

const PAGE_OFFSET: u64 = 0xfff;
// Arbitrary ROM limit for now
const MAX_ROM_SIZE: usize = 0x20_0000;

fn build_instance(
    name: &str,
    max_cpu: u8,
    lowmem: usize,
    highmem: usize,
    log: slog::Logger,
    rt_handle: Handle,
) -> Result<Arc<Instance>> {
    let mut builder = Builder::new(
        name,
        propolis::vmm::CreateOpts { force: true, ..Default::default() },
    )?
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
    Instance::create(builder.finalize()?, Some(rt_handle), Some(log))
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

fn build_log() -> (slog::Logger, slog_async::AsyncGuard) {
    let decorator = slog_term::TermDecorator::new().build();
    let drain = slog_term::CompactFormat::new(decorator).build().fuse();
    let (drain, guard) = slog_async::Async::new(drain).build_with_guard();
    (slog::Logger::root(drain.fuse(), o!()), guard)
}

fn setup_instance(
    log: slog::Logger,
    config: config::Config,
    rt_handle: Handle,
) -> anyhow::Result<Arc<Instance>> {
    let vm_name = config.get_name();
    let cpus = config.get_cpus();

    const GB: usize = 1024 * 1024 * 1024;
    const MB: usize = 1024 * 1024;
    let memsize: usize = config.get_mem() * MB;
    let lowmem = memsize.min(3 * GB);
    let highmem = memsize.saturating_sub(3 * GB);

    let inst =
        build_instance(vm_name, cpus, lowmem, highmem, log.clone(), rt_handle)
            .context("Failed to create VM Instance")?;
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

        let rtc = &machine.kernel_devs.rtc;
        rtc.memsize_to_nvram(lowmem, highmem, mctx.hdl())?;
        rtc.set_time(SystemTime::now(), mctx.hdl())?;

        let hdl = machine.get_hdl();
        let pci_builder = propolis::hw::pci::topology::Builder::new();
        let chipset = hw::chipset::i440fx::I440Fx::create(
            machine,
            pci_builder.finish(inv, &machine.bus_pio, &machine.bus_mmio)?,
            Default::default(),
        );
        inv.register(&chipset)?;

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
        inv.register_instance(&com1, "com1")?;
        inv.register_instance(&com2, "com2")?;
        inv.register_instance(&com3, "com3")?;
        inv.register_instance(&com4, "com4")?;

        // PS/2
        let ps2_ctrl = PS2Ctrl::create();
        ps2_ctrl.attach(pio, chipset.as_ref());
        inv.register(&ps2_ctrl)?;

        let debug_file = std::fs::File::create("debug.out")?;
        let debug_out = chardev::BlockingFileOutput::new(debug_file);
        let debug_device = hw::qemu::debug::QemuDebugPort::create(pio);
        debug_out
            .attach(Arc::clone(&debug_device) as Arc<dyn BlockingSource>, disp);
        inv.register(&debug_device)?;

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

                    let (backend, creg) = config.block_dev(block_dev, disp);
                    let bdf = bdf.unwrap();

                    let info = backend.info();
                    let vioblk = hw::virtio::PciVirtioBlock::new(0x100, info);
                    let id = inv.register_instance(&vioblk, bdf.to_string())?;
                    let _be_id = inv.register_child(creg, id)?;

                    backend.attach(
                        vioblk.clone() as Arc<dyn block::Device>,
                        disp,
                    )?;

                    chipset.pci_attach(bdf, vioblk);
                }
                "pci-virtio-viona" => {
                    let vnic_name =
                        dev.options.get("vnic").unwrap().as_str().unwrap();
                    let bdf = bdf.unwrap();

                    let viona = hw::virtio::PciVirtioViona::new(
                        vnic_name, 0x100, &hdl,
                    )?;
                    inv.register_instance(&viona, bdf.to_string())?;
                    chipset.pci_attach(bdf, viona);
                }
                "pci-nvme" => {
                    let block_dev =
                        dev.options.get("block_dev").unwrap().as_str().unwrap();

                    let (backend, creg) = config.block_dev(block_dev, disp);
                    let bdf = bdf.unwrap();

                    let info = backend.info();
                    let nvme =
                        hw::nvme::PciNvme::create(block_dev.to_string(), info);

                    let id = inv.register_instance(&nvme, bdf.to_string())?;
                    let _be_id = inv.register_child(creg, id)?;

                    backend.attach(nvme.clone(), disp)?;

                    chipset.pci_attach(bdf, nvme);
                }
                _ => {
                    slog::error!(log, "unrecognized driver"; "name" => name);
                    return Err(Error::new(
                        ErrorKind::Other,
                        "Unrecognized driver",
                    ));
                }
            }
        }

        let mut fwcfg = hw::qemu::fwcfg::FwCfgBuilder::new();
        fwcfg
            .add_legacy(
                hw::qemu::fwcfg::LegacyId::SmpCpuCount,
                hw::qemu::fwcfg::FixedItem::new_u32(cpus as u32),
            )
            .map_err(|err| Error::new(ErrorKind::Other, err))?;

        let ramfb = hw::qemu::ramfb::RamFb::create();
        ramfb.attach(&mut fwcfg);

        let fwcfg_dev = fwcfg.finalize();
        fwcfg_dev.attach(pio);

        inv.register(&fwcfg_dev)?;
        inv.register(&ramfb)?;

        for vcpu in mctx.vcpus() {
            vcpu.set_default_capabs()?;
        }

        Ok(())
    })
    .context("Failed to initialize instance")?;

    inst.spawn_vcpu_workers(propolis::vcpu_run_loop)
        .context("Failed spawn vCPU workers: {}")?;

    drop(romfp);

    inst.print();

    // Wait until someone connects to ttya
    slog::error!(log, "Waiting for a connection to ttya");
    com1_sock.wait_for_connect();

    inst.on_transition(Box::new(move |next_state, _target, _inv, ctx| {
        match next_state {
            State::Boot => {
                for vcpu in ctx.mctx.vcpus() {
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

    Ok(inst)
}

#[derive(clap::Parser)]
/// Propolis command-line frontend for running a VM.
struct Args {
    /// Either the VM config file or a previously captured snapshot image.
    #[clap(value_name = "CONFIG|SNAPSHOT", action)]
    target: String,

    /// Take a snapshot on Ctrl-C before exiting.
    #[clap(short, long, action)]
    snapshot: bool,

    /// Restore previously captured snapshot.
    #[clap(short, long, action)]
    restore: bool,
}

fn main() -> anyhow::Result<()> {
    let Args { target, snapshot, restore } = Args::parse();

    // Ensure proper setup of USDT probes
    register_probes().context("Failed to setup USDT probes")?;

    let (log, _log_async_guard) = build_log();

    // Create tokio runtime, we don't use the tokio::main macro
    // since we'll block in main when we call `Instance::wait_for_state`
    let rt =
        tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    let rt_handle = rt.handle();

    // Create the VM afresh or restore it from a snapshot
    let (config, inst) = if restore {
        todo!("restore VM from snapshot")
    } else {
        let config = config::parse(&target)?;
        let inst =
            setup_instance(log.clone(), config.clone(), rt_handle.clone())?;
        (config, inst)
    };

    // Register a Ctrl-C handler so we can snapshot before exiting if needed
    let inst_weak = Arc::downgrade(&inst);
    let signal_log = log.clone();
    let signal_rt_handle = rt_handle.clone();
    ctrlc::set_handler(move || {
        static SNAPSHOT: Once = Once::new();
        if let Some(inst) = inst_weak.upgrade() {
            if snapshot {
                if SNAPSHOT.is_completed() {
                    slog::warn!(signal_log, "snapshot already in progress");
                } else {
                    let snap_log = signal_log.new(o!("task" => "snapshot"));
                    let snap_rt_handle = signal_rt_handle.clone();
                    let config = config.clone();
                    SNAPSHOT.call_once(move || {
                        snap_rt_handle.spawn(async move {
                            if let Err(err) = snapshot::save(
                                snap_log.clone(),
                                inst.clone(),
                                config,
                            )
                            .await
                            .context("Failed to save snapshot of VM")
                            {
                                slog::error!(snap_log, "{:?}", err);
                                let _ = inst.set_target_state(ReqState::Halt);
                            }
                        });
                    });
                }
            } else {
                slog::info!(signal_log, "Destroying instance...");
                inst.set_target_state(ReqState::Halt)
                    .expect("failed to stop VM");
            }
        }
    })
    .context("Failed to register Ctrl-C signal handler.")?;

    // Let the VM start and we're off to the races
    slog::info!(log, "Starting instance...");
    inst.set_target_state(ReqState::Run).context("Failed to run VM")?;

    inst.wait_for_state(State::Destroy);
    drop(inst);

    Ok(())
}
