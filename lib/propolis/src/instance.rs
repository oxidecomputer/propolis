//! Structures related VM instances management.

#![allow(unused)]

use std::io;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, Weak};
use std::thread::{self, JoinHandle};

use crate::dispatch::*;
use crate::hw;
use crate::inventory::{self, Inventory};
use crate::vcpu::VcpuRunFunc;
use crate::vmm::*;

use serde::{Deserialize, Serialize};
use slog::{self, Drain};
use thiserror::Error;
use tokio::runtime::Handle;

/// The role of an instance during a migration.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum MigrateRole {
    Source,
    Destination,
}

impl std::fmt::Display for MigrateRole {
    fn fmt(&self, fmt: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MigrateRole::Source => write!(fmt, "source"),
            MigrateRole::Destination => write!(fmt, "destination"),
        }
    }
}

/// Phases an Instance may transition through during a migration.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum MigratePhase {
    /// The initial migration phase an Instance enters.
    Start,

    /// Wind down the instance and give devices a chance to complete in-flight requests.
    Pause,

    /// Devices and vCPUs have been paused
    Paused,
}

/// States of operation for an instance.
#[derive(Copy, Clone, Eq, PartialEq, Debug)]
pub enum State {
    /// Initial state. Instances cannot return to this state
    /// after transitioning away from it.
    Initialize,
    /// The instance is booting.
    Boot,
    /// The instance is actively running.
    Run,
    /// The instance is in a paused state such that it may
    /// later be booted or maintained.
    Quiesce,
    /// The instance is being migrated.
    Migrate(MigrateRole, MigratePhase),
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
                Some(
                    target @ State::Migrate(
                        MigrateRole::Destination,
                        MigratePhase::Start,
                    ),
                ) => target,
                _ => State::Boot,
            },
            State::Boot => match target {
                None | Some(State::Run) => State::Run,
                Some(State::Migrate(MigrateRole::Destination, phase)) => {
                    State::Migrate(MigrateRole::Destination, phase)
                }
                _ => State::Quiesce,
            },
            State::Run => match target {
                None | Some(State::Run) => State::Run,
                Some(State::Migrate(role, phase)) => {
                    State::Migrate(role, phase)
                }
                Some(_) => State::Quiesce,
            },
            State::Quiesce => match target {
                Some(State::Halt) | Some(State::Destroy) => State::Halt,
                Some(State::Reset) => State::Reset,
                // Machine must go through reset before it can be booted
                Some(State::Boot) => State::Reset,
                Some(State::Migrate(role, phase)) => {
                    State::Migrate(role, phase)
                }
                _ => State::Quiesce,
            },
            State::Migrate(MigrateRole::Source, MigratePhase::Start) => {
                match target {
                    Some(State::Migrate(MigrateRole::Source, phase)) => {
                        State::Migrate(MigrateRole::Source, phase)
                    }
                    _ => {
                        State::Migrate(MigrateRole::Source, MigratePhase::Start)
                    }
                }
            }
            State::Migrate(role, phase) => match target {
                Some(State::Run) => State::Run,
                Some(State::Halt) | Some(State::Destroy) => State::Quiesce,
                Some(State::Migrate(target_role, target_phase))
                    if *role == target_role =>
                {
                    State::Migrate(target_role, target_phase)
                }
                _ => State::Migrate(*role, *phase),
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

/// An instance state transition can be broken down into different phases
/// visible to consumers.
#[derive(Copy, Clone, Eq, PartialEq, Debug)]
pub enum TransitionPhase {
    Pre,
    Post,
}

/// States which external consumers are permitted to request that the instance
/// transition to.
#[derive(Copy, Clone, Eq, PartialEq, Debug)]
pub enum ReqState {
    Run,
    Reset,
    Halt,
    MigrateStart,
    MigrateResume,
}

/// Errors that may be returned when an instance is requested to transition
/// to a different state.
#[derive(Debug, Error)]
pub enum TransitionError {
    #[error("cannot reset instance while halted")]
    ResetWhileHalted,

    #[error("cannot transition from {current:?} state to {target:?}")]
    InvalidTarget { current: State, target: State },

    #[error("cannot transition away from terminal state")]
    Terminal,

    #[error("an outstanding migration task already exists")]
    MigrationAlreadyInProgress,
}

type TransitionFunc =
    dyn Fn(State, Option<State>, &Inventory, &DispCtx) + Send + Sync + 'static;

/// Context used when moving through migration states.
#[derive(Default)]
struct MigrateCtx {
    /// The dispatcher ID of the task that's driving the migration. This task
    /// must remain unblocked while quiescing for migration to proceed.
    task_id: Option<CtxId>,

    /// This event is signaled when the state driver has finished sending pause
    /// requests to all entities that receive them.
    pause_requests_sent: Option<Arc<tokio::sync::Notify>>,

    /// The migration task writes to this channel to commit a transition to the
    /// migrate-paused state.
    pause_chan: Option<std::sync::mpsc::Receiver<()>>,
}

struct Inner {
    state_current: State,
    state_target: Option<State>,
    suspend_info: Option<(SuspendKind, SuspendSource)>,
    drive_thread: Option<JoinHandle<()>>,
    machine: Option<Arc<Machine>>,
    inv: Arc<Inventory>,
    transition_funcs: Vec<Box<TransitionFunc>>,
    migrate_ctx: MigrateCtx,
}

/// A single virtual machine.
pub struct Instance {
    inner: Mutex<Inner>,
    cv: Condvar,
    // Some tests still reach in to access the dispatcher
    pub(crate) disp: Arc<Dispatcher>,
    logger: slog::Logger,
    me: Weak<Instance>,
}
impl Instance {
    /// Creates a new virtual machine, absorbing `machine` generated from
    /// a `machine::Builder`.
    pub fn create(
        machine: Arc<Machine>,
        rt_handle: Option<Handle>,
        logger: Option<slog::Logger>,
    ) -> io::Result<Arc<Self>> {
        let logger = logger
            .unwrap_or_else(|| slog::Logger::root(slog::Discard, slog::o!()));
        let driver_log = logger.new(slog::o!("task" => "instance-driver"));

        let this = Arc::new_cyclic(|me| {
            let mweak = Arc::downgrade(&machine);
            Self {
                inner: Mutex::new(Inner {
                    state_current: State::Initialize,
                    state_target: None,
                    suspend_info: None,
                    drive_thread: None,
                    machine: Some(machine),
                    inv: Arc::new(Inventory::new()),
                    transition_funcs: Vec::new(),
                    migrate_ctx: Default::default(),
                }),
                cv: Condvar::new(),
                disp: Dispatcher::new(me.clone(), mweak, rt_handle),
                logger,
                me: me.clone(),
            }
        });

        // Register bhyve-internal devices
        let mut state = this.inner.lock().unwrap();
        let machine = state.machine.as_ref().unwrap();
        machine.kernel_devs.register(&state.inv);
        for vcpu in machine.vcpus.iter() {
            state.inv.register_instance(vcpu, vcpu.cpuid());
        }
        drop(state);

        // Fire up instance state driver task
        let driver_hdl = Arc::clone(&this);
        let driver = thread::Builder::new()
            .name("instance-driver".to_string())
            .spawn(move || driver_hdl.drive_state(driver_log))?;
        let mut state = this.inner.lock().unwrap();
        state.drive_thread = Some(driver);
        drop(state);

        Ok(this)
    }

    /// Spawn vCPU worker threads, using `vcpu_fn` to drive handling of VM exits
    ///
    /// # Panics
    ///
    /// - Panics if the instance's state is not [`State::Initialize`].
    pub fn spawn_vcpu_workers(&self, vcpu_fn: VcpuRunFunc) -> io::Result<()> {
        self.initialize(|machine, _mctx, disp, inv| {
            for vcpu in machine.vcpus.iter() {
                let vcpu_id = vcpu.cpuid() as usize;
                let name = format!("vcpu-{}", vcpu_id);

                let arc_vcpu = vcpu.clone();
                let func = Box::new(move |sctx: &mut SyncCtx| {
                    vcpu_fn(&arc_vcpu, sctx);
                });
                let wake = Box::new(move |ctx: &DispCtx| {
                    let _ = ctx.mctx.vcpu(vcpu_id).barrier();
                });

                let _ = disp.spawn_sync(name, func, Some(wake))?;
            }
            Ok(())
        })
    }

    /// Acquire an `AsyncCtx`, useful for accessing instance state from
    /// emulation running in an async runtime
    pub fn async_ctx(&self) -> AsyncCtx {
        self.disp.async_ctx()
    }

    pub fn logger(&self) -> &slog::Logger {
        &self.logger
    }

    /// Invokes `func`, which may operate on the instance's internal state
    /// to prepare an instance before it boots.
    ///
    /// # Panics
    ///
    /// - Panics if the instance's state is not [`State::Initialize`].
    pub fn initialize<F>(&self, func: F) -> io::Result<()>
    where
        F: FnOnce(
            &Machine,
            &MachineCtx,
            &Dispatcher,
            &Inventory,
        ) -> io::Result<()>,
    {
        let state = self.inner.lock().unwrap();
        assert_eq!(state.state_current, State::Initialize);
        let machine = state.machine.as_ref().unwrap();
        let mctx = MachineCtx::new(machine.clone());
        let rt_guard = self.disp.handle().unwrap().enter();
        func(machine, &mctx, &self.disp, &state.inv)
    }

    /// Returns the state of the instance.
    pub fn current_state(&self) -> State {
        let state = self.inner.lock().unwrap();
        let res = state.state_current;
        drop(state);
        res
    }

    /// Updates the state of the instance.
    ///
    /// Returns an error if the state transition is invalid.
    pub fn set_target_state(
        &self,
        target: ReqState,
    ) -> Result<(), TransitionError> {
        let mut inner = self.inner.lock().unwrap();

        if matches!(inner.state_target, Some(State::Halt | State::Destroy)) {
            // Cannot request any state once the target is halt/destroy
            return Err(TransitionError::Terminal);
        }
        if inner.state_target == Some(State::Reset) && target == ReqState::Run {
            // Requesting a run when already on the road to reboot is an
            // immediate success.
            return Ok(());
        }

        match target {
            ReqState::Run => {
                self.set_target_state_locked(&mut inner, State::Run)
            }
            ReqState::Reset => self.trigger_suspend_locked(
                &mut inner,
                SuspendKind::Reset,
                SuspendSource::External,
            ),
            ReqState::Halt => self.trigger_suspend_locked(
                &mut inner,
                SuspendKind::Halt,
                SuspendSource::External,
            ),
            ReqState::MigrateStart => self.set_target_state_locked(
                &mut inner,
                State::Migrate(MigrateRole::Source, MigratePhase::Start),
            ),
            ReqState::MigrateResume => self.set_target_state_locked(
                &mut inner,
                State::Migrate(MigrateRole::Destination, MigratePhase::Start),
            ),
        }
    }

    /// Asks the instance to pause in preparation for a migration. Returns a
    /// tokio Notify that the state driver signals when all entities have
    /// been sent a pause request.
    ///
    /// This will ask the instance to transition to the Migrate Pause
    /// state. As part of that, we'll walk through the device inventory
    /// asking each to stop servicing guest requests. The instance will
    /// wait while the caller has the opportunity to monitor the progress
    /// of each device. The instance expects the caller to inform when it
    /// should complete the transition to the Migrate Pause state via the
    /// passed in Receiver.
    pub fn migrate_pause(
        &self,
        migrate_ctx_id: CtxId,
        pause_chan: std::sync::mpsc::Receiver<()>,
    ) -> Result<Arc<tokio::sync::Notify>, TransitionError> {
        let mut inner = self.inner.lock().unwrap();

        // Make sure the Instance is in the appropriate state
        if !matches!(
            inner.state_current,
            State::Migrate(MigrateRole::Source, MigratePhase::Start)
        ) {
            return Err(TransitionError::InvalidTarget {
                current: inner.state_current,
                target: State::Migrate(
                    MigrateRole::Source,
                    MigratePhase::Pause,
                ),
            });
        }

        // And that another migration wasn't already requested
        if let Some(_) = inner.migrate_ctx.task_id {
            return Err(TransitionError::MigrationAlreadyInProgress);
        }

        // Stash the migrate context and pause channel so that
        // `drive_state` can access them
        inner.migrate_ctx.task_id = Some(migrate_ctx_id);
        inner.migrate_ctx.pause_chan = Some(pause_chan);
        let notify = Arc::new(tokio::sync::Notify::new());
        inner.migrate_ctx.pause_requests_sent = Some(notify.clone());

        self.set_target_state_locked(
            &mut inner,
            State::Migrate(MigrateRole::Source, MigratePhase::Pause),
        )?;

        Ok(notify)
    }

    pub(crate) fn trigger_suspend(
        &self,
        kind: SuspendKind,
        source: SuspendSource,
    ) -> Result<(), TransitionError> {
        let mut inner = self.inner.lock().unwrap();
        self.trigger_suspend_locked(&mut inner, kind, source)
    }

    fn trigger_suspend_locked(
        &self,
        inner: &mut MutexGuard<Inner>,
        kind: SuspendKind,
        source: SuspendSource,
    ) -> Result<(), TransitionError> {
        if matches!(inner.state_current, State::Halt | State::Destroy) {
            // No way out from Halt or Destroy
            return Err(TransitionError::Terminal);
        }

        match kind {
            SuspendKind::Reset => {
                match inner.suspend_info {
                    Some((SuspendKind::Halt, _)) => {
                        // Cannot supersede active halt
                        return Err(TransitionError::ResetWhileHalted);
                    }
                    Some((SuspendKind::Reset, _)) => {
                        return Ok(());
                    }
                    _ => {}
                }

                let hdl = inner.machine.as_ref().unwrap().get_hdl();
                let _ =
                    hdl.suspend(bhyve_api::vm_suspend_how::VM_SUSPEND_RESET);
                inner.suspend_info = Some((kind, source));
                self.set_target_state_locked(inner, State::Reset)
            }
            SuspendKind::Halt => {
                if matches!(inner.suspend_info, Some((SuspendKind::Halt, _))) {
                    return Ok(());
                }
                if inner.suspend_info.is_none() {
                    let hdl = inner.machine.as_ref().unwrap().get_hdl();
                    let _ = hdl.suspend(
                        bhyve_api::vm_suspend_how::VM_SUSPEND_POWEROFF,
                    );
                }
                inner.suspend_info = Some((kind, source));
                self.set_target_state_locked(inner, State::Halt)
            }
        }
    }

    fn set_target_state_locked(
        &self,
        inner: &mut MutexGuard<Inner>,
        target: State,
    ) -> Result<(), TransitionError> {
        if !State::valid_target(&inner.state_current, &target) {
            return Err(TransitionError::InvalidTarget {
                current: inner.state_current,
                target,
            });
        }
        // XXX: verify validity of transitions
        inner.state_target = Some(target);
        self.cv.notify_all();
        Ok(())
    }

    /// Blocks the calling thread until the machine reaches
    /// the state `target` or [`State::Destroy`].
    pub fn wait_for_state(&self, target: State) {
        let mut state = self.inner.lock().unwrap();
        self.cv.wait_while(state, |state| {
            // bail if we reach the target state or Destroy
            state.state_current != target
                && state.state_current != State::Destroy
        });
    }

    /// Registers  callback, `func`, which is invoked whenever a state
    /// transition occurs.
    pub fn on_transition(&self, func: Box<TransitionFunc>) {
        let mut state = self.inner.lock().unwrap();
        state.transition_funcs.push(func);
    }

    fn transition_actions(
        &self,
        inner: &MutexGuard<Inner>,
        state: State,
        target: Option<State>,
        phase: TransitionPhase,
    ) {
        self.disp.with_ctx(|ctx| {
            // Allow any entity to act on the new state
            inner.inv.for_each_node(inventory::Order::Pre, |_id, rec| {
                let ent = rec.entity();

                // Entities using the `reset` shortcut will be notified of the
                // new state in the `Pre` phase, so they can complete any
                // clean-up or reset activities before the in-kernel state is
                // reinitialized.
                if state == State::Reset && phase == TransitionPhase::Pre {
                    ent.reset(ctx);
                }

                // Inform each entity to stop servicing the guest and
                // complete or cancel any outstanding requests.
                if state
                    == State::Migrate(MigrateRole::Source, MigratePhase::Pause)
                    && phase == TransitionPhase::Pre
                {
                    ent.pause(ctx);
                }

                ent.state_transition(state, target, phase, ctx);

                Ok::<_, ()>(())
            });

            // Transition-func consumers only expect post-state-change
            // notifications for now.
            if phase == TransitionPhase::Post {
                for f in inner.transition_funcs.iter() {
                    f(state, target, &inner.inv, ctx)
                }
            }
        });
    }

    fn drive_state(&self, log: slog::Logger) {
        let mut next_state: Option<State> = None;
        let mut inner = self.inner.lock().unwrap();

        // Run with the context of the tokio runtime availble
        let rt_guard = self.disp.handle().unwrap().enter();

        loop {
            if next_state.is_none() {
                if let Some(t) = inner.state_target {
                    if t == inner.state_current {
                        inner.state_target = None;
                        continue;
                    }
                    next_state =
                        inner.state_current.next_transition(inner.state_target);
                } else {
                    // Nothing to do but wait for a new target
                    inner = self.cv.wait(inner).unwrap();
                    continue;
                }
            }
            let state = next_state.take().unwrap();

            let prev_state = inner.state_current;
            inner.state_current = state;
            slog::info!(log, "Instance transition";
                "state" => ?state,
                "state_prev" => ?prev_state,
                "state_target" => ?&inner.state_target
            );
            if matches!(&inner.state_target, Some(s) if *s == state) {
                // target state has been reached
                inner.state_target = None;
            }

            // Pre-state-change actions
            self.transition_actions(
                &inner,
                state,
                inner.state_target,
                TransitionPhase::Pre,
            );

            // Implicit actions for a state change
            match state {
                State::Boot => {
                    // Set vCPUs to their proper boot (INIT) state
                    for vcpu in &inner.machine.as_ref().unwrap().vcpus {
                        vcpu.reboot_state().unwrap();
                        vcpu.activate().unwrap();
                        // Set BSP to start up
                        if vcpu.is_bsp() {
                            vcpu.set_run_state(bhyve_api::VRS_RUN, None)
                                .unwrap();
                            vcpu.set_reg(
                                bhyve_api::vm_reg_name::VM_REG_GUEST_RIP,
                                0xfff0,
                            )
                            .unwrap();
                        }
                    }
                }
                State::Migrate(
                    MigrateRole::Destination,
                    MigratePhase::Start,
                ) => {
                    // Activate the vCPUs. We leave setting the vCPU state to the import logic
                    for vcpu in &inner.machine.as_ref().unwrap().vcpus {
                        vcpu.activate().unwrap();
                    }
                }
                State::Quiesce => {
                    // Worker thread quiesce cannot be done with `inner` lock
                    // held without risking a deadlock.
                    drop(inner);
                    self.disp.quiesce();
                    inner = self.inner.lock().unwrap();
                }
                State::Destroy => {
                    // Like quiesce, worker destruction should not be done with
                    // `inner` lock held.
                    drop(inner);
                    self.disp.shutdown();
                    inner = self.inner.lock().unwrap();
                }
                State::Reset => {
                    inner.machine.as_ref().unwrap().reinitialize().unwrap();
                }
                State::Run => {
                    // Upon entry to the Run state, details about any previous
                    // suspend become stale.
                    inner.suspend_info = None;

                    self.disp.release();
                }
                State::Migrate(MigrateRole::Source, MigratePhase::Pause) => {
                    // Notify the migration control thread that all entity pause
                    // requests have been sent.
                    let notify = inner
                        .migrate_ctx
                        .pause_requests_sent
                        .take()
                        .expect(
                        "Entity pause notifier should be set during migration",
                    );
                    notify.notify_one();

                    // Give an opportunity to migration requestor to take
                    // action before quiescing the instance.
                    // Drop the `inner` lock so that it may access instance
                    // in the meanwhile.
                    let pause_chan = inner
                        .migrate_ctx
                        .pause_chan
                        .take()
                        .expect("migrate pause channel");
                    drop(inner);
                    let pause = pause_chan.recv();
                    inner = self.inner.lock().unwrap();
                    if let Err(_) = pause {
                        // The other end is gone without waking us first, bail out
                        slog::warn!(log, "migrate pause chan dropped early");
                        inner.state_target = Some(State::Halt);
                        continue;
                    }
                }
                _ => {}
            }

            // Post-state-change actions
            self.transition_actions(
                &inner,
                state,
                inner.state_target,
                TransitionPhase::Post,
            );

            // Implicit post-state-change actions
            match state {
                State::Boot => {
                    // A reset is as good as fulfilled when transitioning
                    // through the Boot state.
                    if inner.state_target == Some(State::Reset) {
                        inner.state_target = None;
                    }
                }
                State::Destroy => {}
                State::Migrate(MigrateRole::Source, MigratePhase::Pause) => {
                    let migrate_ctx = inner.migrate_ctx.task_id.unwrap();
                    // Worker thread quiesce cannot be done with `inner` lock
                    // held without risking a deadlock.
                    drop(inner);
                    self.disp.quiesce();
                    // We explicitly allow the migrate task to run
                    self.disp.release_one(migrate_ctx);
                    inner = self.inner.lock().unwrap();

                    // All the vCPUs and devices should be paused by this point
                    // so update the target state to indicate as such
                    self.set_target_state_locked(
                        &mut inner,
                        State::Migrate(
                            MigrateRole::Source,
                            MigratePhase::Paused,
                        ),
                    )
                    .unwrap();
                }
                _ => {}
            }

            // Notify any waiters about the completed state-change
            self.cv.notify_all();

            // Bail if completely destroyed
            if matches!(state, State::Destroy) {
                break;
            }

            next_state = state.next_transition(inner.state_target);
        }

        // Explicitly drop the instance-held reference to the Machine.  If all
        // the worker tasks (sync or async) are complete, this should be the
        // last holder and result in its destruction.
        let _ = inner.machine.take();
    }

    pub fn print(&self) {
        let state = self.inner.lock().unwrap();
        state.inv.print()
    }

    /// Return the [`Inventory`] describing the device tree of this Instance.s
    pub fn inv(&self) -> Arc<Inventory> {
        let state = self.inner.lock().unwrap();
        Arc::clone(&state.inv)
    }
}

impl Drop for Instance {
    fn drop(&mut self) {
        let mut state = self.inner.lock().unwrap();
        if state.state_current != State::Destroy {
            drop(state);
            self.set_target_state(ReqState::Halt).unwrap();
            state = self.inner.lock().unwrap();
            state = self
                .cv
                .wait_while(state, |state| {
                    state.state_current != State::Destroy
                })
                .unwrap();
        }
        let join_handle = state.drive_thread.take().unwrap();
        // The last Instance handle may be held by the drive_thread itself
        // which would mean a deadlock or error if we tried to join
        if std::thread::current().id() != join_handle.thread().id() {
            let _joined = join_handle.join();
        }
    }
}

#[cfg(test)]
impl Instance {
    pub fn new_test(rt_handle: Option<Handle>) -> io::Result<Arc<Self>> {
        let drain = slog_term::FullFormat::new(
            slog_term::PlainSyncDecorator::new(slog_term::TestStdoutWriter),
        )
        .build()
        .fuse();
        let logger = slog::Logger::root(drain, slog::o!());

        Self::create(Machine::new_test()?, rt_handle, Some(logger))
    }
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum SuspendKind {
    Reset,
    Halt,
}
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum SuspendSource {
    /// Triple-fault from vCPU ID
    TripleFault(i32),
    /// Initiated from named device
    Device(&'static str),
    /// External request (power/reset button)
    External,
}
