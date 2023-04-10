use std::collections::VecDeque;
use std::fs::File;
use std::io::{Error, ErrorKind, Result};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Context;
use clap::Parser;
use futures::future::BoxFuture;
use propolis::chardev::{BlockingSource, Sink, Source, UDSock};
use propolis::hw::chipset::{i440fx, Chipset};
use propolis::hw::ibmpc;
use propolis::hw::ps2::ctrl::PS2Ctrl;
use propolis::hw::uart::LpcUart;
use propolis::intr_pins::FuncPin;
use propolis::vcpu::Vcpu;
use propolis::vmm::{Builder, Machine};
use propolis::*;
use tokio::runtime;

use propolis::usdt::register_probes;

use slog::{o, Drain};

mod config;
mod snapshot;

const PAGE_OFFSET: u64 = 0xfff;
// Arbitrary ROM limit for now
const MAX_ROM_SIZE: usize = 0x20_0000;

#[derive(Copy, Clone, Debug)]
enum InstEvent {
    Halt,
    ReqHalt,

    Reset,
    TripleFault,

    ReqSave,

    ReqStart,
}
impl InstEvent {
    fn priority(&self) -> u8 {
        match self {
            InstEvent::Halt | InstEvent::ReqHalt => 3,

            InstEvent::Reset | InstEvent::TripleFault => 2,

            InstEvent::ReqSave => 1,

            InstEvent::ReqStart => 0,
        }
    }
    fn supersedes(&self, comp: &Self) -> bool {
        self.priority() >= comp.priority()
    }
}

#[derive(Clone, Debug)]
enum EventCtx {
    Vcpu(i32),
    Pin(String),
    User(String),
    Other(String),
}

#[derive(Default)]
struct EQInner {
    events: VecDeque<(InstEvent, EventCtx)>,
}
#[derive(Default)]
struct EventQueue {
    inner: Mutex<EQInner>,
    cv: Condvar,
}
impl EventQueue {
    fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }
    fn push(&self, ev: InstEvent, ctx: EventCtx) {
        let mut inner = self.inner.lock().unwrap();
        inner.events.push_back((ev, ctx));
        self.cv.notify_one();
    }
    fn pop_superseding(
        &self,
        cur: Option<&InstEvent>,
    ) -> Option<(InstEvent, EventCtx)> {
        let mut inner = self.inner.lock().unwrap();
        while let Some((ev, ctx)) = inner.events.pop_front() {
            match cur {
                Some(cur_ev) => {
                    if cur_ev.supersedes(&ev) {
                        // queued event is superseded by current one, so discard
                        // it and look for another which may be relevant.
                        continue;
                    } else {
                        return Some((ev, ctx));
                    }
                }
                None => return Some((ev, ctx)),
            }
        }
        None
    }
    fn wait(&self) {
        let guard = self.inner.lock().unwrap();
        let _guard = self.cv.wait_while(guard, |g| g.events.is_empty());
    }
}

#[derive(Copy, Clone, Eq, PartialEq, Debug)]
pub enum State {
    /// Initial state.
    Initialize,
    /// The instance is actively running.
    Run,
    /// The instance is in a paused state such that it may
    /// later be booted or maintained.
    Quiesce,
    /// The instance state is being exported
    Save,
    /// The instance is no longer running
    Halt,
    /// The instance is rebooting, and should transition back
    /// to the "Run" state.
    Reset,
    /// Terminal state in which the instance is torn down.
    Destroy,
}
impl State {
    fn next(&self, ev: InstEvent) -> (Self, Option<InstEvent>) {
        match self {
            State::Initialize => {
                if matches!(ev, InstEvent::ReqStart) {
                    (State::Run, None)
                } else {
                    // All other events require a quiesce first
                    (State::Quiesce, Some(ev))
                }
            }
            State::Run => {
                if matches!(ev, InstEvent::ReqStart) {
                    // Discard any duplicate start requests when running
                    (State::Run, None)
                } else {
                    // All other events require a quiesce first
                    (State::Quiesce, Some(ev))
                }
            }
            State::Quiesce => match ev {
                InstEvent::Halt | InstEvent::ReqHalt => (State::Halt, Some(ev)),
                InstEvent::Reset | InstEvent::TripleFault => {
                    (State::Reset, Some(ev))
                }
                InstEvent::ReqSave => (State::Save, Some(ev)),
                InstEvent::ReqStart => {
                    // Reaching quiesce with a "start" event would be odd
                    panic!("unexpected ReqStart");
                }
            },
            State::Save => (State::Halt, Some(ev)),
            State::Halt => (State::Destroy, None),
            State::Reset => match ev {
                InstEvent::Halt | InstEvent::ReqHalt => (State::Halt, Some(ev)),
                InstEvent::Reset | InstEvent::TripleFault => (State::Run, None),
                _ => (State::Run, Some(ev)),
            },
            State::Destroy => (State::Destroy, None),
        }
    }
}

struct InstState {
    instance: Option<propolis::Instance>,
    state: State,
    vcpu_tasks: Vec<propolis::tasks::TaskCtrl>,
}

struct InstInner {
    state: Mutex<InstState>,
    boot_gen: AtomicUsize,
    eq: Arc<EventQueue>,
    cv: Condvar,
    config: config::Config,
}

struct Instance(Arc<InstInner>);
impl Instance {
    fn new(
        pinst: propolis::Instance,
        config: config::Config,
        from_restore: bool,
        log: slog::Logger,
    ) -> Self {
        let this = Self(Arc::new(InstInner {
            state: Mutex::new(InstState {
                instance: Some(pinst),
                state: State::Initialize,
                vcpu_tasks: Vec::new(),
            }),
            boot_gen: AtomicUsize::new(0),
            eq: EventQueue::new(),
            cv: Condvar::new(),
            config,
        }));

        // Some gymnastics required for the split borrow through the MutexGuard
        let mut state_guard = this.0.state.lock().unwrap();
        let state = &mut *state_guard;
        let guard = state.instance.as_ref().unwrap().lock();

        for vcpu in guard.machine().vcpus.iter().map(Arc::clone) {
            let (task, ctrl) =
                propolis::tasks::TaskHdl::new_held(Some(vcpu.barrier_fn()));

            let inner = this.0.clone();
            let task_log = log.new(slog::o!("vcpu" => vcpu.id));
            let _ = std::thread::Builder::new()
                .name(format!("vcpu-{}", vcpu.id))
                .spawn(move || {
                    Instance::vcpu_loop(inner, vcpu.as_ref(), &task, task_log)
                })
                .unwrap();
            state.vcpu_tasks.push(ctrl);
        }
        drop(guard);
        drop(state_guard);

        let rt_hdl = runtime::Handle::current();
        let inner = this.0.clone();
        let state_log = log.clone();
        let _ = std::thread::Builder::new()
            .name("state loop".to_string())
            .spawn(move || {
                // Make sure the instance state driver has access to tokio
                let _rt_guard = rt_hdl.enter();
                Instance::state_loop(inner, from_restore, state_log)
            })
            .unwrap();

        this
    }

    fn device_state_transition(
        state: State,
        inst_guard: &propolis::instance::InstanceGuard,
        first_boot: bool,
    ) {
        let inv_guard = inst_guard.inventory().lock();

        for (_eid, record) in inv_guard.iter(propolis::inventory::Order::Pre) {
            let ent = record.entity();
            match state {
                State::Run => {
                    if first_boot {
                        ent.start().unwrap_or_else(|_| {
                            panic!("entity {} failed to start", record.name())
                        });
                    } else {
                        ent.resume();
                    }
                }
                State::Quiesce => ent.pause(),
                State::Halt => ent.halt(),
                State::Reset => ent.reset(),
                _ => panic!("invalid device state transition {:?}", state),
            }
        }
        if matches!(state, State::Quiesce) {
            let tasks: futures::stream::FuturesUnordered<
                BoxFuture<'static, ()>,
            > = inv_guard
                .iter(propolis::inventory::Order::Pre)
                .map(|(_eid, record)| record.entity().paused())
                .collect();
            drop(inv_guard);

            // Wait for all of the pause futures to complete
            tokio::runtime::Handle::current().block_on(async move {
                use futures::stream::StreamExt;
                let _: Vec<()> = tasks.collect().await;
            });
        }
    }

    fn state_loop(
        inner: Arc<InstInner>,
        from_restore: bool,
        log: slog::Logger,
    ) {
        let mut guard = inner.state.lock().unwrap();
        let mut cur_ev = None;

        if !from_restore {
            // Initialized vCPUs to standard x86 state, unless this instance is
            // being restored from a snapshot, in which case the snapshot state
            // will be injected prior to start-up.
            let inst = guard.instance.as_ref().unwrap().lock();
            inst.machine().vcpu_x86_setup().unwrap();
            drop(inst);
        }

        // If instance was restored from previously-saved state, the kernel VMM
        // portion will be paused so it could be consistently loaded.  Issue the
        // necessary resume before attempting to run.
        let mut needs_resume = from_restore;

        assert!(matches!(guard.state, State::Initialize));
        loop {
            if let Some((next_ev, ctx)) =
                inner.eq.pop_superseding(cur_ev.as_ref())
            {
                slog::info!(&log, "Instance event {:?} ({:?})", next_ev, ctx);
                cur_ev = Some(next_ev);
            }

            if cur_ev.is_none() {
                drop(guard);
                inner.eq.wait();
                guard = inner.state.lock().unwrap();
                continue;
            }

            let (next_state, resid_ev) = guard.state.next(cur_ev.unwrap());
            if guard.state == next_state {
                continue;
            }

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
                State::Run => {
                    // start device emulation and vCPUs
                    let inst = guard.instance.as_ref().unwrap().lock();
                    Self::device_state_transition(
                        State::Run,
                        &inst,
                        inner.boot_gen.load(Ordering::Acquire) == 0,
                    );
                    if needs_resume {
                        inst.machine()
                            .hdl
                            .resume()
                            .expect("restored instance can resume running");
                        needs_resume = false;
                    }
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
                    Self::device_state_transition(State::Quiesce, &inst, false);
                    inst.machine().hdl.pause().expect("pause should complete");
                }
                State::Save => {
                    let guard = &mut *guard;
                    let inst = guard.instance.as_ref().unwrap();
                    let save_res =
                        tokio::runtime::Handle::current().block_on(async {
                            snapshot::save(inst, &inner.config, &log).await
                        });
                    if let Err(err) = save_res {
                        slog::error!(log, "Snapshot error {:?}", err);
                    }
                }
                State::Halt => {
                    let guard = &mut *guard;
                    let inst = guard.instance.as_ref().unwrap().lock();
                    Self::device_state_transition(State::Halt, &inst, false);
                    for mut vcpu_ctrl in guard.vcpu_tasks.drain(..) {
                        vcpu_ctrl.exit();
                    }
                }
                State::Reset => {
                    let inst = guard.instance.as_ref().unwrap().lock();
                    Self::device_state_transition(State::Reset, &inst, false);
                    inst.machine().reinitialize().unwrap();
                    inst.machine().vcpu_x86_setup().unwrap();
                    inner.boot_gen.fetch_add(1, Ordering::Release);
                    inst.machine()
                        .hdl
                        .resume()
                        .expect("resume should complete");
                }
                State::Destroy => {
                    // Drop the instance
                    let _ = guard.instance.take().unwrap();

                    // Communicate that destruction is complete
                    slog::info!(&log, "Instance destroyed");
                    guard.state = State::Destroy;
                    inner.cv.notify_all();
                    return;
                }
            }
            guard.state = next_state;
            cur_ev = resid_ev;
        }
    }

    fn wait_destroyed(&self) {
        let guard = self.0.state.lock().unwrap();
        let _guard = self
            .0
            .cv
            .wait_while(guard, |g| !matches!(g.state, State::Destroy))
            .unwrap();
    }

    fn vcpu_loop(
        inner: Arc<InstInner>,
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
                    let cur_gen = inner.boot_gen.load(Ordering::Acquire);
                    if local_gen != cur_gen {
                        // Reset occurred, discard any existing entry details.
                        entry = VmEntry::Run;
                        local_gen = cur_gen;
                    }
                    continue;
                }
                Some(Event::Exit) => {
                    return;
                }
                None => {}
            }
            entry = match propolis::vcpu_process(vcpu, &entry, &log) {
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
                    VmError::Suspended(suspend) => {
                        let ctx = EventCtx::Vcpu(vcpu.id);
                        let ev = match suspend {
                            exits::Suspend::Halt => InstEvent::Halt,
                            exits::Suspend::Reset => InstEvent::Reset,
                            exits::Suspend::TripleFault => {
                                InstEvent::TripleFault
                            }
                        };
                        inner.eq.push(ev, ctx);
                        task.force_hold();

                        // The next entry is unimportant as we have queued a
                        // significant event and halted this vCPU task with the
                        // expectation that it will be acted upon soon.
                        VmEntry::Run
                    }
                    VmError::Io(e) => {
                        slog::error!(&log, "VM entry error {:?}", e,);

                        inner.eq.push(
                            InstEvent::Halt,
                            EventCtx::Other(format!(
                                "error {:?} on vcpu {}",
                                e.raw_os_error().unwrap_or(0),
                                vcpu.id
                            )),
                        );
                        task.force_hold();

                        VmEntry::Run
                    }
                },
            };
        }
    }

    fn generate_pins(&self) -> (Arc<FuncPin>, Arc<FuncPin>) {
        let power_eq = self.0.eq.clone();
        let power_pin =
            propolis::intr_pins::FuncPin::new(Box::new(move |rising| {
                if rising {
                    power_eq.push(
                        InstEvent::Halt,
                        EventCtx::Pin("power pin".to_string()),
                    );
                }
            }));
        let reset_eq = self.0.eq.clone();
        let reset_pin =
            propolis::intr_pins::FuncPin::new(Box::new(move |rising| {
                if rising {
                    reset_eq.push(
                        InstEvent::Reset,
                        EventCtx::Pin("reset pin".to_string()),
                    );
                }
            }));
        (Arc::new(power_pin), Arc::new(reset_pin))
    }

    fn lock(&self) -> Option<InnerGuard> {
        let guard = self.0.state.lock().unwrap();
        guard.instance.as_ref()?;
        Some(InnerGuard(guard))
    }
    fn eq(&self) -> Arc<EventQueue> {
        self.0.eq.clone()
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
    config: config::Config,
    from_restore: bool,
    log: &slog::Logger,
) -> anyhow::Result<(Instance, Arc<UDSock>)> {
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
    let inst = Instance::new(pinst, config.clone(), from_restore, log.clone());
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
    rtc.set_time(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time precedes UNIX epoch"),
    )?;

    let (power_pin, reset_pin) = inst.generate_pins();
    let pci_topo =
        propolis::hw::pci::topology::Builder::new().finish(inv, machine)?;
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
                let (backend, creg) = config::block_backend(&config, dev, log);
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
                let (backend, creg) = config::block_backend(&config, dev, log);
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

/// Check bhyve and viona API versions, squawking if they do not meet
/// expectations, but ultimately still allowing forward progress since
/// propolis-standalone lives in the Thunderdome.
fn api_version_checks(log: &slog::Logger) -> std::io::Result<()> {
    match api_version::check() {
        Err(api_version::Error::Io(e)) => {
            // IO errors _are_ fatal
            Err(e)
        }
        Err(api_version::Error::Mismatch(comp, act, exp)) => {
            // Make noise about version mismatch, but soldier on and let the
            // user decide if they want to quit
            slog::error!(
                log,
                "{} API version mismatch {} != {}",
                comp,
                act,
                exp
            );
            Ok(())
        }
        Ok(_) => Ok(()),
    }
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

    // Check that vmm and viona device version match what we expect
    api_version_checks(&log).context("API version checks")?;

    // Create tokio runtime, we don't use the tokio::main macro
    // since we'll block in main when we call `Instance::wait_for_state`
    let rt =
        tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    let _rt_guard = rt.enter();

    // Create the VM afresh or restore it from a snapshot
    let (inst, com1_sock) = if restore {
        let (inst, com1_sock) =
            rt.block_on(async { snapshot::restore(&target, &log).await })?;
        (inst, com1_sock)
    } else {
        let config = config::parse(&target)?;
        let (inst, com1_sock) = setup_instance(config, false, &log)?;
        (inst, com1_sock)
    };

    // Register a Ctrl-C handler so we can snapshot before exiting if needed
    let ctrlc_eq = inst.eq();
    let signal_log = log.clone();
    let mut ctrlc_fired = false;
    ctrlc::set_handler(move || {
        if ctrlc_fired {
            return;
        } else {
            ctrlc_fired = true;
        }
        if snapshot {
            ctrlc_eq
                .push(InstEvent::ReqSave, EventCtx::User("Ctrl+C".to_string()));
        } else {
            slog::info!(signal_log, "Destroying instance...");
            ctrlc_eq
                .push(InstEvent::ReqHalt, EventCtx::User("Ctrl+C".to_string()));
        }
    })
    .context("Failed to register Ctrl-C signal handler.")?;

    // Wait until someone connects to ttya
    slog::info!(log, "Waiting for a connection to ttya");
    com1_sock.wait_for_connect();

    // Let the VM start and we're off to the races
    slog::info!(log, "Starting instance...");
    inst.eq().push(
        InstEvent::ReqStart,
        EventCtx::User("UDS connection".to_string()),
    );

    // wait for instance to be destroyed
    inst.wait_destroyed();
    Ok(())
}
