// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::collections::{BTreeMap, VecDeque};
use std::fmt;
use std::fs::File;
use std::io::{Error, ErrorKind, Result};
use std::path::Path;
use std::process::ExitCode;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Context;
use clap::Parser;
use futures::future::BoxFuture;
use propolis::hw::qemu::pvpanic::QemuPvpanic;
use slog::{o, Drain};
use strum::IntoEnumIterator;
use tokio::runtime;

use propolis::chardev::{BlockingSource, Sink, Source, UDSock};
use propolis::hw::chipset::{i440fx, Chipset};
use propolis::hw::ps2::ctrl::PS2Ctrl;
use propolis::hw::uart::LpcUart;
use propolis::hw::{ibmpc, qemu};
use propolis::intr_pins::FuncPin;
use propolis::usdt::register_probes;
use propolis::vcpu::Vcpu;
use propolis::vmm::{Builder, Machine};
use propolis::*;

mod cidata;
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
impl From<propolis::exits::Suspend> for InstEvent {
    fn from(value: propolis::exits::Suspend) -> Self {
        match value {
            exits::Suspend::Halt => Self::Halt,
            exits::Suspend::Reset => Self::Reset,
            exits::Suspend::TripleFault(_) => Self::TripleFault,
        }
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

#[derive(Default)]
struct Inventory {
    devs: BTreeMap<String, Arc<dyn propolis::common::Lifecycle>>,
    block: BTreeMap<String, Arc<dyn propolis::block::Backend>>,
}
impl Inventory {
    fn register<D: propolis::common::Lifecycle>(&mut self, dev: &Arc<D>) {
        self.devs.insert(
            dev.type_name().into(),
            dev.clone() as Arc<dyn propolis::common::Lifecycle>,
        );
    }
    fn register_instance<D: propolis::common::Lifecycle>(
        &mut self,
        dev: &Arc<D>,
        name: &str,
    ) {
        self.devs.insert(
            format!("{}-{name}", dev.type_name()),
            dev.clone() as Arc<dyn propolis::common::Lifecycle>,
        );
    }
    fn register_block(
        &mut self,
        be: &Arc<dyn propolis::block::Backend>,
        name: String,
    ) {
        self.block.insert(name, be.clone());
    }
}

struct InstState {
    machine: Option<propolis::Machine>,
    inventory: Inventory,
    state: State,
    vcpu_tasks: Vec<propolis::tasks::TaskCtrl>,
    exit_code: Option<u8>,
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
        machine: propolis::Machine,
        config: config::Config,
        from_restore: bool,
        log: slog::Logger,
    ) -> Self {
        let this = Self(Arc::new(InstInner {
            state: Mutex::new(InstState {
                machine: Some(machine),
                inventory: Inventory::default(),
                state: State::Initialize,
                vcpu_tasks: Vec::new(),
                exit_code: None,
            }),
            boot_gen: AtomicUsize::new(0),
            eq: EventQueue::new(),
            cv: Condvar::new(),
            config,
        }));

        // Some gymnastics required for the split borrow through the MutexGuard
        let mut state_guard = this.0.state.lock().unwrap();
        let state = &mut *state_guard;
        let machine = state.machine.as_ref().unwrap();

        for vcpu in machine.vcpus.iter().map(Arc::clone) {
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
        guard: &MutexGuard<InstState>,
        first_boot: bool,
    ) {
        for (name, device) in guard.inventory.devs.iter() {
            match state {
                State::Run => {
                    if first_boot {
                        device.start().unwrap_or_else(|_| {
                            panic!("device {} failed to start", name)
                        });
                    } else {
                        device.resume();
                    }
                }
                State::Quiesce => device.pause(),
                State::Halt => device.halt(),
                State::Reset => device.reset(),
                _ => panic!("invalid device state transition {:?}", state),
            }
        }
        if matches!(state, State::Quiesce) {
            let tasks: futures::stream::FuturesUnordered<
                BoxFuture<'static, ()>,
            > = guard
                .inventory
                .devs
                .values()
                .map(|device| device.paused())
                .collect();

            // Wait for all of the pause futures to complete
            tokio::runtime::Handle::current().block_on(async move {
                use futures::stream::StreamExt;
                let _: Vec<()> = tasks.collect().await;
            });
        }

        // Drive block backends through their necessary states too
        match state {
            State::Run if first_boot => {
                for (_name, be) in guard.inventory.block.iter() {
                    be.start().expect("blockdev start succeeds");
                }
            }
            State::Halt => {
                for (_name, be) in guard.inventory.block.iter() {
                    be.halt();
                }
            }
            _ => {}
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
            let machine = guard.machine.as_ref().unwrap();
            machine.vcpu_x86_setup().unwrap();
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
                    Self::device_state_transition(
                        State::Run,
                        &guard,
                        inner.boot_gen.load(Ordering::Acquire) == 0,
                    );
                    if needs_resume {
                        let machine = guard.machine.as_ref().unwrap();
                        machine
                            .hdl
                            .resume()
                            .expect("restored instance can resume running");
                        needs_resume = false;
                    }

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
                    Self::device_state_transition(
                        State::Quiesce,
                        &guard,
                        false,
                    );
                    let machine = guard.machine.as_ref().unwrap();
                    machine.hdl.pause().expect("pause should complete");
                }
                State::Save => {
                    let guard = &mut *guard;
                    let save_res = snapshot::save(guard, &inner.config, &log);
                    if let Err(err) = save_res {
                        slog::error!(log, "Snapshot error {:?}", err);
                    }
                }
                State::Halt => {
                    Self::device_state_transition(State::Halt, &guard, false);
                    for mut vcpu_ctrl in guard.vcpu_tasks.drain(..) {
                        vcpu_ctrl.exit();
                    }
                    if guard.exit_code.is_none() {
                        guard.exit_code = Some(inner.config.main.exit_on_halt);
                    }
                }
                State::Reset => {
                    if let (None, Some(code)) =
                        (guard.exit_code, inner.config.main.exit_on_reboot)
                    {
                        // Emit the configured exit-on-reboot code if one is
                        // configured an no existing code would already
                        // supersede it.
                        guard.exit_code = Some(code);
                        guard.state = State::Halt;
                        cur_ev = Some(InstEvent::ReqHalt);
                        continue;
                    }
                    Self::device_state_transition(State::Reset, &guard, false);
                    let machine = guard.machine.as_ref().unwrap();
                    machine.reinitialize().unwrap();
                    machine.vcpu_x86_setup().unwrap();
                    inner.boot_gen.fetch_add(1, Ordering::Release);
                    machine.hdl.resume().expect("resume should complete");
                }
                State::Destroy => {
                    // Drop the machine
                    let _ = guard.machine.take().unwrap();

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

    fn wait_destroyed(&self) -> ExitCode {
        let guard = self.0.state.lock().unwrap();
        let mut guard = self
            .0
            .cv
            .wait_while(guard, |g| !matches!(g.state, State::Destroy))
            .unwrap();
        ExitCode::from(guard.exit_code.take().unwrap_or(0))
    }

    fn vcpu_loop(
        inner: Arc<InstInner>,
        vcpu: &Vcpu,
        task: &propolis::tasks::TaskHdl,
        log: slog::Logger,
    ) {
        use propolis::exits::{SuspendDetail, VmExitKind};
        use propolis::tasks::Event;

        let mut entry = VmEntry::Run;
        let mut exit = VmExit::default();
        let mut local_gen = 0;
        loop {
            let mut exit_when_consistent = false;
            match task.pending_event() {
                Some(Event::Hold) => {
                    if !exit.kind.is_consistent() {
                        // Before the vCPU task can enter the held state, its
                        // associated in-kernel state must be driven to a point
                        // where it is consistent.
                        exit_when_consistent = true;
                    } else {
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
                }
                Some(Event::Exit) => {
                    return;
                }
                None => {}
            }

            exit = match vcpu.run(&entry, exit_when_consistent) {
                Err(e) => {
                    slog::error!(&log, "VM entry error {:?}", e);

                    inner.eq.push(
                        InstEvent::Halt,
                        EventCtx::Other(format!(
                            "error {:?} on vcpu {}",
                            e.raw_os_error().unwrap_or(0),
                            vcpu.id
                        )),
                    );
                    task.force_hold();

                    entry = VmEntry::Run;
                    continue;
                }
                Ok(exit) => exit,
            };

            entry = vcpu.process_vmexit(&exit).unwrap_or_else(|| {
                match exit.kind {
                    VmExitKind::Inout(pio) => {
                        slog::error!(
                            &log,
                            "Unhandled pio {:x?}", pio; "rip" => exit.rip
                        );
                        VmEntry::InoutFulfill(exits::InoutRes::emulate_failed(
                            &pio,
                        ))
                    }
                    VmExitKind::Mmio(mmio) => {
                        slog::error!(
                            &log,
                            "Unhandled mmio {:x?}", mmio; "rip" => exit.rip
                        );
                        VmEntry::MmioFulfill(exits::MmioRes::emulate_failed(
                            &mmio,
                        ))
                    }
                    VmExitKind::Rdmsr(msr) => {
                        slog::error!(
                            &log,
                            "Unhandled rdmsr {:#08x}", msr; "rip" => exit.rip
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
                            "Unhandled wrmsr {:#08x} <- {:#08x}", msr, val;
                            "rip" => #%exit.rip
                        );
                        VmEntry::Run
                    }
                    VmExitKind::Suspended(SuspendDetail {
                        kind,
                        when: _when,
                    }) => {
                        match kind {
                            exits::Suspend::Halt | exits::Suspend::Reset => {
                                inner
                                    .eq
                                    .push(kind.into(), EventCtx::Vcpu(vcpu.id));
                            }
                            exits::Suspend::TripleFault(vcpuid) => {
                                if vcpuid == -1 || vcpuid == vcpu.id {
                                    inner.eq.push(
                                        kind.into(),
                                        EventCtx::Vcpu(vcpu.id),
                                    );
                                }
                            }
                        }
                        task.force_hold();

                        // The next entry is unimportant as we have queued a
                        // significant event and halted this vCPU task with the
                        // expectation that it will be acted upon soon.
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
                }
            });
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

    fn lock(&self) -> Option<MutexGuard<'_, InstState>> {
        let guard = self.0.state.lock().unwrap();
        // Make sure machine is still "live"
        guard.machine.as_ref()?;
        Some(guard)
    }
    fn eq(&self) -> Arc<EventQueue> {
        self.0.eq.clone()
    }
}

fn build_machine(
    name: &str,
    max_cpu: u8,
    lowmem: usize,
    highmem: usize,
    use_reservoir: bool,
) -> Result<propolis::Machine> {
    let mut builder = Builder::new(
        name,
        propolis::vmm::CreateOpts {
            force: true,
            use_reservoir,
            ..Default::default()
        },
    )?
    .max_cpus(max_cpu)?
    .add_mem_region(0, lowmem, "lowmem")?
    .add_rom_region(0x1_0000_0000 - MAX_ROM_SIZE, MAX_ROM_SIZE, "bootrom")?
    .add_mmio_region(0xc000_0000, 0x2000_0000, "dev32")?
    .add_mmio_region(0xe000_0000, 0x1000_0000, "pcicfg")?;

    let highmem_start = 0x1_0000_0000;
    if highmem > 0 {
        builder = builder.add_mem_region(highmem_start, highmem, "highmem")?;
    }

    let dev64_start = highmem_start + highmem;
    builder = builder.add_mmio_region(
        dev64_start,
        vmm::MAX_PHYSMEM - dev64_start,
        "dev64",
    )?;

    builder.finalize()
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

fn build_log(level: slog::Level) -> slog::Logger {
    let main_drain = if atty::is(atty::Stream::Stdout) {
        let decorator = slog_term::TermDecorator::new().build();
        let drain = slog_term::CompactFormat::new(decorator).build().fuse();
        slog_async::Async::new(drain)
            .overflow_strategy(slog_async::OverflowStrategy::Block)
            .build_no_guard()
    } else {
        let drain =
            slog_bunyan::with_name("propolis-standalone", std::io::stdout())
                .build()
                .fuse();
        slog_async::Async::new(drain)
            .overflow_strategy(slog_async::OverflowStrategy::Block)
            .build_no_guard()
    };

    let (dtrace_drain, probe_reg) = slog_dtrace::Dtrace::new();

    let filtered_main = slog::LevelFilter::new(main_drain, level);

    let log = slog::Logger::root(
        slog::Duplicate::new(filtered_main.fuse(), dtrace_drain.fuse()).fuse(),
        o!(),
    );

    if let slog_dtrace::ProbeRegistration::Failed(err) = probe_reg {
        slog::error!(&log, "Error registering slog-dtrace probes: {:?}", err);
    }

    log
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

    let use_reservoir = config.main.use_reservoir.unwrap_or(false);
    if use_reservoir {
        // Do a quick check of the reservoir size if asked to use it
        //
        // The actual VM create can TOCTOU race, but we can at least raise the
        // issue nicely if things are way off.
        let ctl = propolis::bhyve_api::VmmCtlFd::open()?;
        let resv_info = ctl.reservoir_query()?;
        if resv_info.vrq_free_sz < memsize {
            slog::warn!(
                log,
                "Reservoir lacks free capacity ({}MiB < {}MiB)",
                resv_info.vrq_free_sz / MB,
                memsize / MB
            );
        }
    }

    slog::info!(log, "Creating VM with {} vCPUs, {} lowmem, {} highmem",
        cpus, lowmem, highmem;);
    let machine = build_machine(vm_name, cpus, lowmem, highmem, use_reservoir)
        .context("Failed to create VM Machine")?;
    let inst =
        Instance::new(machine, config.clone(), from_restore, log.clone());
    slog::info!(log, "VM created"; "name" => vm_name);

    let (romfp, rom_len) =
        open_bootrom(&config.main.bootrom).context("Cannot open bootrom")?;
    let com1_sock =
        UDSock::bind(Path::new("./ttya")).context("Cannot open UD socket")?;

    // Get necessary access to innards, now that it is nestled in `Instance`
    let mut inst_guard = inst.lock().unwrap();
    // Split borrows require this dance
    let guard = &mut *inst_guard;
    let machine = guard.machine.as_ref().unwrap();
    let hdl = machine.hdl.clone();

    populate_rom(machine, "bootrom", &romfp, rom_len)?;
    drop(romfp);

    // Add vCPUs to inventory, since they count as devices
    for vcpu in machine.vcpus.iter() {
        guard.inventory.register_instance(vcpu, &vcpu.id.to_string())
    }

    let (power_pin, reset_pin) = inst.generate_pins();
    let pci_topo =
        propolis::hw::pci::topology::Builder::new().finish(machine)?.topology;

    let chipset_hb = i440fx::I440FxHostBridge::create(
        pci_topo,
        i440fx::Opts {
            power_pin: Some(power_pin),
            reset_pin: Some(reset_pin),
            ..Default::default()
        },
    );
    let chipset_lpc = i440fx::Piix3Lpc::create(machine.hdl.clone());
    let chipset_pm = i440fx::Piix3PM::create(
        machine.hdl.clone(),
        chipset_hb.power_pin(),
        log.new(slog::o!("device" => "piix3pm")),
    );

    let chipset_pci_attach = |bdf, pcidev| {
        chipset_hb.pci_attach(bdf, pcidev, chipset_lpc.route_lintr(bdf));
    };

    chipset_pci_attach(i440fx::DEFAULT_HB_BDF, chipset_hb.clone());
    chipset_pci_attach(i440fx::DEFAULT_LPC_BDF, chipset_lpc.clone());
    chipset_pci_attach(i440fx::DEFAULT_PM_BDF, chipset_pm.clone());

    chipset_hb.attach(machine);
    chipset_lpc.attach(&machine.bus_pio);
    chipset_pm.attach(&machine.bus_pio);

    guard.inventory.register(&chipset_hb);
    guard.inventory.register(&chipset_lpc);
    guard.inventory.register(&chipset_pm);

    // RTC: populate time and CMOS
    let rtc = chipset_lpc.rtc.as_ref();
    rtc.memsize_to_nvram(lowmem as u32, highmem as u64)?;
    rtc.set_time(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time precedes UNIX epoch"),
    )?;

    // HPET
    let hpet = propolis::hw::bhyve::BhyveHpet::create(hdl.clone());
    guard.inventory.register(&hpet);

    // UARTs
    let com1 = LpcUart::new(chipset_lpc.irq_pin(ibmpc::IRQ_COM1).unwrap());
    let com2 = LpcUart::new(chipset_lpc.irq_pin(ibmpc::IRQ_COM2).unwrap());
    let com3 = LpcUart::new(chipset_lpc.irq_pin(ibmpc::IRQ_COM3).unwrap());
    let com4 = LpcUart::new(chipset_lpc.irq_pin(ibmpc::IRQ_COM4).unwrap());

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
    guard.inventory.register_instance(&com1, "com1");
    guard.inventory.register_instance(&com2, "com2");
    guard.inventory.register_instance(&com3, "com3");
    guard.inventory.register_instance(&com4, "com4");

    // PS/2
    let ps2_ctrl = PS2Ctrl::create();
    ps2_ctrl.attach(
        pio,
        chipset_lpc.irq_pin(ibmpc::IRQ_PS2_PRI).unwrap(),
        chipset_lpc.irq_pin(ibmpc::IRQ_PS2_AUX).unwrap(),
        chipset_hb.reset_pin(),
    );
    guard.inventory.register(&ps2_ctrl);

    let debug_file = std::fs::File::create("debug.out")?;
    let debug_out = chardev::BlockingFileOutput::new(debug_file);
    let debug_device = hw::qemu::debug::QemuDebugPort::create(pio);
    debug_out.attach(Arc::clone(&debug_device) as Arc<dyn BlockingSource>);
    guard.inventory.register(&debug_device);

    for (name, dev) in config.devices.iter() {
        let driver = &dev.driver as &str;
        slog::debug!(log, "creating device"; "name" => ?name, "driver" => %driver);

        let bdf = if driver.starts_with("pci-") {
            config::parse_bdf(
                dev.options.get("pci-path").unwrap().as_str().unwrap(),
            )
        } else {
            None
        };
        let mut create_device = || -> anyhow::Result<()> {
            match driver {
                "pci-virtio-block" => {
                    let (backend, name) =
                        config::block_backend(&config, dev, log);
                    let bdf = bdf.unwrap();

                    let vioblk = hw::virtio::PciVirtioBlock::new(0x100);

                    guard
                        .inventory
                        .register_instance(&vioblk, &bdf.to_string());
                    guard.inventory.register_block(&backend, name);

                    block::attach(backend, vioblk.clone());
                    chipset_pci_attach(bdf, vioblk);
                }
                "pci-virtio-viona" => {
                    let vnic_name =
                        dev.options.get("vnic").unwrap().as_str().unwrap();
                    let bdf = bdf.unwrap();

                    let viona = hw::virtio::PciVirtioViona::new(
                        vnic_name, 0x100, &hdl,
                    )?;
                    guard.inventory.register_instance(&viona, &bdf.to_string());
                    chipset_pci_attach(bdf, viona);
                }
                "pci-nvme" => {
                    let (backend, name) =
                        config::block_backend(&config, dev, log);
                    let bdf = bdf.unwrap();

                    let dev_serial = dev
                        .options
                        .get("block_dev")
                        .unwrap()
                        .as_str()
                        .unwrap()
                        .to_string();
                    let log =
                        log.new(slog::o!("dev" => format!("nvme-{}", name)));
                    let nvme = hw::nvme::PciNvme::create(dev_serial, log);

                    guard.inventory.register_instance(&nvme, &bdf.to_string());
                    guard.inventory.register_block(&backend, name);

                    block::attach(backend, nvme.clone());
                    chipset_pci_attach(bdf, nvme);
                }
                qemu::pvpanic::DEVICE_NAME => {
                    let enable_isa = dev
                        .options
                        .get("enable_isa")
                        .and_then(|opt| opt.as_bool())
                        .unwrap_or(false);
                    if enable_isa {
                        let pvpanic = QemuPvpanic::create(
                            log.new(slog::o!("dev" => "pvpanic")),
                        );
                        pvpanic.attach_pio(pio);
                        guard.inventory.register(&pvpanic);
                    }
                }
                _ => {
                    slog::error!(log, "unrecognized driver {driver}"; "name" => name);
                    return Err(Error::new(
                        ErrorKind::Other,
                        "Unrecognized driver",
                    )
                    .into());
                }
            };
            Ok(())
        };
        create_device().with_context(|| {
            format!("Failed to create {driver} device '{name}'")
        })?;
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

    guard.inventory.register(&fwcfg_dev);
    guard.inventory.register(&ramfb);

    let cpuid_profile = config::parse_cpuid(&config)?;

    for vcpu in machine.vcpus.iter() {
        let vcpu_profile = if let Some(profile) = cpuid_profile.as_ref() {
            propolis::cpuid::Specializer::new()
                .with_vcpu_count(
                    std::num::NonZeroU8::new(config.main.cpus).unwrap(),
                    true,
                )
                .with_vcpuid(vcpu.id)
                .with_cache_topo()
                .clear_cpu_topo(cpuid::TopoKind::iter())
                .execute(profile.clone())
                .context("failed to specialize cpuid profile")?
        } else {
            // An empty set will instruct the kernel to use the legacy
            // fallback behavior
            propolis::cpuid::Set::new_host()
        };
        vcpu.set_cpuid(vcpu_profile)?;
        vcpu.set_default_capabs()?;
    }
    drop(inst_guard);

    Ok((inst, com1_sock))
}

/// Check bhyve and viona API versions, squawking if they do not meet
/// expectations, but ultimately still allowing forward progress since
/// propolis-standalone lives in the Thunderdome.
fn api_version_checks(log: &slog::Logger) -> std::io::Result<()> {
    match api_version::check() {
        Err(api_version::Error::Io(e)) => {
            if e.kind() == ErrorKind::NotFound {
                slog::error!(log, "Failed to open /dev/vmmctl");
            }

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

    /// Maximum log level filter.
    #[clap(
        long,
        env = "PROPOLIS_LOG",
        default_value_t = LogFilter(slog::Level::Info),
        ignore_case(true),
    )]
    log_level: LogFilter,
}

fn main() -> anyhow::Result<ExitCode> {
    let Args { target, snapshot, restore, log_level: LogFilter(log_level) } =
        Args::parse();

    // Ensure proper setup of USDT probes
    register_probes().context("Failed to setup USDT probes")?;

    let log = build_log(log_level);

    // Check that vmm and viona device version match what we expect
    api_version_checks(&log).context("API version checks")?;

    // Create tokio runtime, we don't use the tokio::main macro
    // since we'll block in main when we call `Instance::wait_for_state`
    let rt =
        tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    let _rt_guard = rt.enter();

    // Create the VM afresh or restore it from a snapshot
    let (inst, com1_sock) = if restore {
        let (inst, com1_sock) = snapshot::restore(&target, &log)?;
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
    Ok(inst.wait_destroyed())
}

/// Wrapper around `slog::Level` to implement `clap::ValueEnum`, so that this
/// type can be parsed from the command line.
#[derive(Clone, Debug)]
struct LogFilter(slog::Level);

impl clap::ValueEnum for LogFilter {
    fn value_variants<'a>() -> &'a [Self] {
        &[
            LogFilter(slog::Level::Critical),
            LogFilter(slog::Level::Error),
            LogFilter(slog::Level::Warning),
            LogFilter(slog::Level::Info),
            LogFilter(slog::Level::Debug),
            LogFilter(slog::Level::Trace),
        ]
    }

    fn to_possible_value(&self) -> Option<clap::builder::PossibleValue> {
        Some(clap::builder::PossibleValue::new(self.0.as_str()))
    }
}

impl fmt::Display for LogFilter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}
