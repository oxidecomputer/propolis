// Required for USDT
#![cfg_attr(
    all(feature = "dtrace-probes", target_os = "macos"),
    feature(asm_sym)
)]

use std::fs::File;
use std::io::{Error, ErrorKind, Result};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, Weak};
use std::time::SystemTime;

use anyhow::Context;
use clap::Parser;
use propolis::chardev::{BlockingSource, Sink, Source, UDSock};
use propolis::hw::chipset::{i440fx, Chipset};
use propolis::hw::ibmpc;
use propolis::hw::ps2ctrl::PS2Ctrl;
use propolis::hw::uart::LpcUart;
use propolis::intr_pins::FuncPin;
use propolis::vcpu::Vcpu;
use propolis::vmm::{Builder, Machine};
use propolis::*;

use propolis::usdt::register_probes;

use slog::{o, Drain};

mod config;
// set aside for now
//mod snapshot;

const PAGE_OFFSET: u64 = 0xfff;
// Arbitrary ROM limit for now
const MAX_ROM_SIZE: usize = 0x20_0000;

#[derive(Copy, Clone, Eq, PartialEq, Debug)]
pub enum State {
    /// Initial state.
    Initialize,
    /// The instance is booting.
    Boot,
    /// The instance is actively running.
    Run,
    /// The instance is in a paused state such that it may
    /// later be booted or maintained.
    Quiesce,
    /// The instance is no longer running
    Halt,
    /// The instance is rebooting, and should transition back
    /// to the "Boot" state.
    Reset,
    /// Terminal state in which the instance is torn down.
    Destroy,
}
impl State {
    fn valid_target(from: &Self, to: &Self) -> bool {
        if from == to {
            return true;
        }
        match (from, to) {
            // Anything can set us on the road to destruction
            (_, State::Destroy) => true,
            // State begins at initialize, but never returns to it
            (_, State::Initialize) => false,
            // State ends at Destroy and cannot leave
            (State::Destroy, _) => false,
            // Halt can only go to destroy (covered above), nothing else
            (State::Halt, _) => false,

            // XXX: more exclusions?
            (_, _) => true,
        }
    }
    fn next_transition(&self, target: Option<Self>) -> Option<Self> {
        if let Some(t) = &target {
            assert!(Self::valid_target(self, t));
        }
        let next = match self {
            State::Initialize => match target {
                Some(State::Halt) | Some(State::Destroy) => State::Quiesce,
                None => State::Initialize,
                _ => State::Boot,
            },
            State::Boot => match target {
                None => State::Boot,
                Some(State::Run) => State::Run,
                _ => State::Quiesce,
            },
            State::Run => match target {
                None | Some(State::Run) => State::Run,
                Some(_) => State::Quiesce,
            },
            State::Quiesce => match target {
                Some(State::Halt) | Some(State::Destroy) => State::Halt,
                Some(State::Reset) => State::Reset,
                // Machine must go through reset before it can be booted
                Some(State::Boot) => State::Reset,
                _ => State::Quiesce,
            },
            State::Halt => State::Destroy,
            State::Reset => State::Boot,
            State::Destroy => State::Destroy,
        };

        if next == *self {
            None
        } else {
            Some(next)
        }
    }
}

struct InstState {
    instance: Option<propolis::Instance>,
    state: State,
    target: Option<State>,
    vcpu_tasks: Vec<propolis::tasks::TaskCtrl>,
}

struct Instance {
    state: Mutex<InstState>,
    boot_generation: AtomicUsize,
    cv: Condvar,
}
impl Instance {
    fn new(pinst: propolis::Instance, log: slog::Logger) -> Arc<Self> {
        let this = Arc::new(Self {
            state: Mutex::new(InstState {
                instance: Some(pinst),
                state: State::Initialize,
                target: None,
                vcpu_tasks: Vec::new(),
            }),
            boot_generation: AtomicUsize::new(0),
            cv: Condvar::new(),
        });

        // Some gymnastics required for the split borrow through the MutexGuard
        let mut state_guard = this.state.lock().unwrap();
        let state = &mut *state_guard;
        let guard = state.instance.as_ref().unwrap().lock();

        for vcpu in guard.machine().vcpus.iter().map(Arc::clone) {
            let (task, ctrl) =
                propolis::tasks::TaskHdl::new_held(Some(vcpu.barrier_fn()));
            let inst_ref = Arc::downgrade(&this);
            let task_log = log.new(slog::o!("vcpu" => vcpu.id));
            let _ = std::thread::Builder::new()
                .name(format!("vcpu-{}", vcpu.id))
                .spawn(move || {
                    Instance::vcpu_loop(
                        &inst_ref,
                        vcpu.as_ref(),
                        &task,
                        task_log,
                    )
                })
                .unwrap();
            state.vcpu_tasks.push(ctrl);
        }
        drop(guard);
        drop(state_guard);

        let state_ref = this.clone();
        let state_log = log.clone();
        let _ = std::thread::Builder::new()
            .name("state loop".to_string())
            .spawn(move || Instance::state_loop(state_ref.as_ref(), state_log))
            .unwrap();

        this
    }

    fn set_target(&self, target: State) {
        let mut guard = self.state.lock().unwrap();
        if State::valid_target(&guard.state, &target) {
            guard.target = Some(target);
        }
        self.cv.notify_all();
    }

    fn device_state_transition(
        state: State,
        iguard: &propolis::instance::InstanceGuard,
    ) {
        let _ = iguard
            .inventory()
            .for_each_node(
                propolis::inventory::Order::Pre,
                |_eid, record| -> std::result::Result<(), ()> {
                    let ent = record.entity();
                    match state {
                        State::Run => ent.start(),
                        State::Quiesce => ent.pause(),
                        State::Halt => ent.halt(),
                        State::Reset => ent.reset(),
                        _ => panic!(
                            "invalid device state transition {:?}",
                            state
                        ),
                    }
                    Ok(())
                },
            )
            .unwrap();
    }

    fn state_loop(&self, log: slog::Logger) {
        let mut guard = self.state.lock().unwrap();
        assert!(matches!(guard.state, State::Initialize));
        loop {
            guard = self
                .cv
                .wait_while(guard, |g| {
                    g.state.next_transition(g.target).is_none()
                })
                .unwrap();
            let next_state = guard.state.next_transition(guard.target).unwrap();
            slog::info!(
                &log,
                "State transition {:?} -> {:?}",
                guard.state,
                next_state
            );
            match next_state {
                State::Initialize => {
                    panic!("initialize state should not be visited again")
                }
                State::Boot => {
                    let inst = guard.instance.as_ref().unwrap().lock();
                    for vcpu in inst.machine().vcpus.iter() {
                        vcpu.activate().unwrap();
                        vcpu.reboot_state().unwrap();
                        if vcpu.is_bsp() {
                            vcpu.set_run_state(bhyve_api::VRS_RUN, None)
                                .unwrap();
                            vcpu.set_reg(
                                bhyve_api::vm_reg_name::VM_REG_GUEST_RIP,
                                0xfff0,
                            )
                            .unwrap()
                        }
                    }
                }
                State::Run => {
                    // start device emulation and vCPUs
                    let inst = guard.instance.as_ref().unwrap().lock();
                    Self::device_state_transition(State::Run, &inst);
                    drop(inst);

                    // TODO: bail if any vCPU tasks have exited already
                    for vcpu_task in guard.vcpu_tasks.iter_mut() {
                        let _ = vcpu_task.run();
                    }
                }
                State::Quiesce => {
                    // stop device emulation and vCPUs
                    for vcpu_task in guard.vcpu_tasks.iter_mut() {
                        let _ = vcpu_task.hold();
                    }
                    let inst = guard.instance.as_ref().unwrap().lock();
                    Self::device_state_transition(State::Quiesce, &inst);
                }
                State::Halt => {
                    let guard = &mut *guard;
                    let inst = guard.instance.as_ref().unwrap().lock();
                    Self::device_state_transition(State::Halt, &inst);
                    for mut vcpu_ctrl in guard.vcpu_tasks.drain(..) {
                        vcpu_ctrl.exit();
                    }
                }
                State::Reset => {
                    let inst = guard.instance.as_ref().unwrap().lock();
                    Self::device_state_transition(State::Reset, &inst);
                    inst.machine().reinitialize().unwrap();
                    self.boot_generation.fetch_add(1, Ordering::Release);
                }
                State::Destroy => {
                    // Drop the instance
                    let inst = guard.instance.take().unwrap();
                    let machine = inst.destroy();
                    let hdl = machine.destroy();
                    hdl.destroy().unwrap();

                    // Communicate that destruction is complete
                    slog::info!(&log, "Instance destroyed");
                    guard.state = State::Destroy;
                    self.cv.notify_all();
                    return;
                }
            }
            guard.state = next_state;
        }
    }

    fn wait_destroyed(&self) {
        let guard = self.state.lock().unwrap();
        let _guard = self
            .cv
            .wait_while(guard, |g| !matches!(g.state, State::Destroy))
            .unwrap();
    }

    fn vcpu_loop(
        this: &Weak<Self>,
        vcpu: &Vcpu,
        task: &propolis::tasks::TaskHdl,
        log: slog::Logger,
    ) {
        use propolis::exits::VmExitKind;

        let mut entry = VmEntry::Run;
        let mut local_gen = 0;
        loop {
            use propolis::tasks::Event;
            match task.pending_event() {
                Some(Event::Hold) => {
                    task.hold();

                    // Check if the instance was reinitialized while task was held.
                    if let Some(inst) = this.upgrade() {
                        let cur_gen =
                            inst.boot_generation.load(Ordering::Acquire);
                        if local_gen != cur_gen {
                            // Reset occurred, discard any existing entry details.
                            entry = VmEntry::Run;
                            local_gen = cur_gen;
                        }
                    } else {
                        // Instance is gone, so bail.
                        return;
                    }
                    continue;
                }
                Some(Event::Exit) => {
                    return;
                }
                None => {}
            }
            entry = match propolis::vcpu_process(&vcpu, &entry, &log) {
                Ok(ent) => ent,
                Err(e) => match e {
                    VmError::Unhandled(exit) => match exit.kind {
                        VmExitKind::Inout(pio) => {
                            slog::error!(
                                &log,
                                "Unhandled pio {:?}", pio; "rip" => exit.rip
                            );
                            VmEntry::InoutFulfill(
                                exits::InoutRes::emulate_failed(&pio),
                            )
                        }
                        VmExitKind::Mmio(mmio) => {
                            slog::error!(
                                &log,
                                "Unhandled mmio {:?}", mmio; "rip" => exit.rip
                            );
                            VmEntry::MmioFulfill(
                                exits::MmioRes::emulate_failed(&mmio),
                            )
                        }
                        VmExitKind::Rdmsr(msr) => {
                            slog::error!(
                                &log,
                                "Unhandled rdmsr {:x}", msr; "rip" => exit.rip
                            );
                            let _ = vcpu.set_reg(
                                bhyve_api::vm_reg_name::VM_REG_GUEST_RAX,
                                0,
                            );
                            let _ = vcpu.set_reg(
                                bhyve_api::vm_reg_name::VM_REG_GUEST_RDX,
                                0,
                            );
                            VmEntry::Run
                        }
                        VmExitKind::Wrmsr(msr, val) => {
                            slog::error!(
                                &log,
                                "Unhandled wrmsr {:x} <- {:x}", msr, val;
                                "rip" => exit.rip
                            );
                            VmEntry::Run
                        }
                        _ => {
                            slog::error!(
                                &log,
                                "Unhandled exit @rip:{:08x} {:?}",
                                exit.rip,
                                exit.kind
                            );
                            todo!()
                        }
                    },
                    VmError::Suspended(suspend) => match suspend {
                        exits::Suspend::Halt => todo!(),
                        exits::Suspend::Reset => todo!(),
                        exits::Suspend::TripleFault => todo!(),
                    },
                    VmError::Io(e) => {
                        slog::error!(&log, "VM entry error {:?}", e,);
                        todo!()
                    }
                },
            };
        }
    }

    fn generate_pins(self: &Arc<Self>) -> (Arc<FuncPin>, Arc<FuncPin>) {
        let power_ref = Arc::downgrade(self);
        let power_pin =
            propolis::intr_pins::FuncPin::new(Box::new(move |rising| {
                if rising {
                    if let Some(_inst) = power_ref.upgrade() {
                        todo!("impl power signal")
                    }
                }
            }));
        let reset_ref = Arc::downgrade(self);
        let reset_pin =
            propolis::intr_pins::FuncPin::new(Box::new(move |rising| {
                if rising {
                    if let Some(_inst) = reset_ref.upgrade() {
                        todo!("impl reset signal")
                    }
                }
            }));
        (Arc::new(power_pin), Arc::new(reset_pin))
    }

    fn lock(&self) -> Option<InnerGuard> {
        let guard = self.state.lock().unwrap();
        if guard.instance.is_none() {
            return None;
        }
        Some(InnerGuard(guard))
    }
}

/// Project access to the inner [`propolis::Instance`] in [`Instance`]
struct InnerGuard<'a>(MutexGuard<'a, InstState>);
impl std::ops::Deref for InnerGuard<'_> {
    type Target = propolis::Instance;

    fn deref(&self) -> &propolis::Instance {
        self.0.instance.as_ref().unwrap()
    }
}
impl std::ops::DerefMut for InnerGuard<'_> {
    fn deref_mut(&mut self) -> &mut propolis::Instance {
        self.0.instance.as_mut().unwrap()
    }
}

fn build_instance(
    name: &str,
    max_cpu: u8,
    lowmem: usize,
    highmem: usize,
) -> Result<propolis::Instance> {
    let mut builder = Builder::new(
        name,
        propolis::vmm::CreateOpts { force: true, ..Default::default() },
    )?
    .max_cpus(max_cpu)?
    .add_mem_region(0, lowmem, "lowmem")?
    .add_rom_region(0x1_0000_0000 - MAX_ROM_SIZE, MAX_ROM_SIZE, "bootrom")?
    .add_mmio_region(0xc000_0000, 0x2000_0000, "dev32")?
    .add_mmio_region(0xe000_0000, 0x1000_0000, "pcicfg")?
    .add_mmio_region(
        vmm::MAX_SYSMEM,
        vmm::MAX_PHYSMEM - vmm::MAX_SYSMEM,
        "dev64",
    )?;
    if highmem > 0 {
        builder = builder.add_mem_region(0x1_0000_0000, highmem, "highmem")?;
    }
    Ok(propolis::Instance::create(builder.finalize()?))
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

fn populate_rom(
    machine: &Machine,
    region_name: &str,
    fp: &File,
    len: usize,
) -> std::io::Result<()> {
    let mem = machine.acc_mem.access().unwrap();
    let mapping = mem.direct_writable_region_by_name(region_name)?;

    if mapping.len() < len {
        return Err(Error::new(ErrorKind::InvalidData, "rom too long"));
    }

    let offset = mapping.len() - len;
    let submapping = mapping.subregion(offset, len).unwrap();
    if submapping.pread(fp, len, 0)? != len {
        // TODO: Handle short read
        return Err(Error::new(ErrorKind::InvalidData, "short read"));
    }
    Ok(())
}

fn setup_instance(
    config: &config::Config,
    log: &slog::Logger,
) -> anyhow::Result<(Arc<Instance>, Arc<UDSock>)> {
    let vm_name = &config.main.name;
    let cpus = config.main.cpus;

    const GB: usize = 1024 * 1024 * 1024;
    const MB: usize = 1024 * 1024;
    let memsize: usize = config.main.memory * MB;
    let lowmem = memsize.min(3 * GB);
    let highmem = memsize.saturating_sub(3 * GB);

    slog::info!(log, "Creating VM with {} vCPUs, {} lowmem, {} highmem",
        cpus, lowmem, highmem;);
    let pinst = build_instance(vm_name, cpus, lowmem, highmem)
        .context("Failed to create VM Instance")?;
    let inst = Instance::new(pinst, log.clone());
    slog::info!(log, "VM created"; "name" => vm_name);

    let (romfp, rom_len) =
        open_bootrom(&config.main.bootrom).context("Cannot open bootrom")?;
    let com1_sock =
        UDSock::bind(Path::new("./ttya")).context("Cannot open UD socket")?;

    // Get necessary access to innards, now that it is nestled in `Instance`
    let inst_inner = inst.lock().unwrap();
    let guard = inst_inner.lock();
    let inv = guard.inventory();
    let machine = guard.machine();
    let hdl = machine.hdl.clone();

    populate_rom(machine, "bootrom", &romfp, rom_len)?;
    drop(romfp);

    let rtc = &machine.kernel_devs.rtc;
    rtc.memsize_to_nvram(lowmem as u32, highmem as u64)?;
    rtc.set_time(SystemTime::now())?;

    let (power_pin, reset_pin) = inst.generate_pins();
    let pci_topo =
        propolis::hw::pci::topology::Builder::new().finish(inv, &machine)?;
    let chipset = i440fx::I440Fx::create(
        machine,
        pci_topo,
        i440fx::Opts {
            power_pin: Some(power_pin),
            reset_pin: Some(reset_pin),
            ..Default::default()
        },
        log.new(slog::o!("dev" => "chipset")),
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
    );
    com1.set_autodiscard(false);

    // XXX: plumb up com2-4, but until then, just auto-discard
    com2.set_autodiscard(true);
    com3.set_autodiscard(true);
    com4.set_autodiscard(true);

    let pio = &machine.bus_pio;
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
    debug_out.attach(Arc::clone(&debug_device) as Arc<dyn BlockingSource>);
    inv.register(&debug_device)?;

    for (name, dev) in config.devices.iter() {
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
                let (backend, creg) = config::block_backend(&config, dev, &log);
                let bdf = bdf.unwrap();

                let info = backend.info();
                let vioblk = hw::virtio::PciVirtioBlock::new(0x100, info);
                let id = inv.register_instance(&vioblk, bdf.to_string())?;
                let _be_id = inv.register_child(creg, id)?;

                backend.attach(vioblk.clone() as Arc<dyn block::Device>)?;

                chipset.pci_attach(bdf, vioblk);
            }
            "pci-virtio-viona" => {
                let vnic_name =
                    dev.options.get("vnic").unwrap().as_str().unwrap();
                let bdf = bdf.unwrap();

                let viona =
                    hw::virtio::PciVirtioViona::new(vnic_name, 0x100, &hdl)?;
                inv.register_instance(&viona, bdf.to_string())?;
                chipset.pci_attach(bdf, viona);
            }
            "pci-nvme" => {
                let (backend, creg) = config::block_backend(&config, dev, &log);
                let bdf = bdf.unwrap();

                let dev_serial = dev
                    .options
                    .get("block_dev")
                    .unwrap()
                    .as_str()
                    .unwrap()
                    .to_string();
                let info = backend.info();
                let log = log.new(slog::o!("dev" => format!("nvme-{}", name)));
                let nvme = hw::nvme::PciNvme::create(dev_serial, info, log);

                let id = inv.register_instance(&nvme, bdf.to_string())?;
                let _be_id = inv.register_child(creg, id)?;

                backend.attach(nvme.clone())?;

                chipset.pci_attach(bdf, nvme);
            }
            _ => {
                slog::error!(log, "unrecognized driver"; "name" => name);
                return Err(Error::new(
                    ErrorKind::Other,
                    "Unrecognized driver",
                )
                .into());
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

    let ramfb =
        hw::qemu::ramfb::RamFb::create(log.new(slog::o!("dev" => "ramfb")));
    ramfb.attach(&mut fwcfg, &machine.acc_mem);

    let fwcfg_dev = fwcfg.finalize();
    fwcfg_dev.attach(pio, &machine.acc_mem);

    inv.register(&fwcfg_dev)?;
    inv.register(&ramfb)?;

    for vcpu in machine.vcpus.iter() {
        vcpu.set_default_capabs()?;
    }
    drop(guard);
    drop(inst_inner);

    Ok((inst, com1_sock))
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
    let _rt_guard = rt.enter();

    // Create the VM afresh or restore it from a snapshot
    let (_config, inst, com1_sock) = if restore {
        // tokio::task::block_in_place(|| { snapshot::restore(log.clone(), &target) })?;
        todo!("wire up save/restore again")
    } else {
        let config = config::parse(&target)?;
        let (inst, com1_sock) = setup_instance(&config, &log)?;
        (config, inst, com1_sock)
    };

    // Register a Ctrl-C handler so we can snapshot before exiting if needed
    let inst_weak = Arc::downgrade(&inst);
    let signal_log = log.clone();
    // let signal_rt_handle = rt_handle.clone();
    let mut ctrlc_fired = false;
    ctrlc::set_handler(move || {
        // static SNAPSHOT: Once = Once::new();
        if ctrlc_fired {
            return;
        } else {
            ctrlc_fired = true;
        }
        if let Some(inst) = inst_weak.upgrade() {
            if snapshot {
                // if SNAPSHOT.is_completed() {
                //     slog::warn!(signal_log, "snapshot already in progress");
                // } else {
                //     let snap_log = signal_log.new(o!("task" => "snapshot"));
                //     let snap_rt_handle = signal_rt_handle.clone();
                //     let config = config.clone();
                //     SNAPSHOT.call_once(move || {
                //         snap_rt_handle.spawn(async move {
                //             if let Err(err) = snapshot::save(
                //                 snap_log.clone(),
                //                 inst.clone(),
                //                 config,
                //             )
                //             .await
                //             .context("Failed to save snapshot of VM")
                //             {
                //                 slog::error!(snap_log, "{:?}", err);
                //                 let _ = inst.set_target_state(ReqState::Halt);
                //             }
                //         });
                //     });
                // }
                slog::error!(signal_log, "TODO: wire up snapshot");
            } else {
                slog::info!(signal_log, "Destroying instance...");
                inst.set_target(State::Destroy);
            }
        }
    })
    .context("Failed to register Ctrl-C signal handler.")?;

    // Wait until someone connects to ttya
    slog::info!(log, "Waiting for a connection to ttya");
    com1_sock.wait_for_connect();

    // Let the VM start and we're off to the races
    slog::info!(log, "Starting instance...");
    inst.set_target(State::Run);

    // wait for instance to be destroyed
    inst.wait_destroyed();
    Ok(())
}
