//! Structures related VM instances management.

#![allow(unused)]

use std::io;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};

use crate::dispatch::*;
use crate::inventory::{self, Inventory};
use crate::vcpu::VcpuRunFunc;
use crate::vmm::*;

use tokio::runtime::Handle;

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
                _ => State::Boot,
            },
            State::Boot => match target {
                None | Some(State::Run) => State::Run,
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

/// States which external consumers are permitted to request that the instance
/// transition to.
#[derive(Copy, Clone, Eq, PartialEq, Debug)]
pub enum ReqState {
    Run,
    Reset,
    Halt,
}

type TransitionFunc = dyn Fn(State, &DispCtx) + Send + Sync + 'static;

struct Inner {
    state_current: State,
    state_target: Option<State>,
    suspend_info: Option<(SuspendKind, SuspendSource)>,
    drive_thread: Option<JoinHandle<()>>,
    machine: Option<Arc<Machine>>,
    inv: Inventory,
    transition_funcs: Vec<Box<TransitionFunc>>,
}

/// A single virtual machine.
pub struct Instance {
    inner: Mutex<Inner>,
    cv: Condvar,
    pub disp: Arc<Dispatcher>,
}
impl Instance {
    /// Creates a new virtual machine, absorbing the supplied `builder`.
    ///
    /// Uses `vcpu_fn` to determine how to run a virtual CPU for the instance.
    pub fn create(
        builder: Builder,
        rt_handle: Option<Handle>,
        vcpu_fn: VcpuRunFunc,
    ) -> io::Result<Arc<Self>> {
        let machine = Arc::new(builder.finalize()?);
        let disp = Dispatcher::new(&machine, rt_handle);

        let this = Arc::new(Self {
            inner: Mutex::new(Inner {
                state_current: State::Initialize,
                state_target: None,
                suspend_info: None,
                drive_thread: None,
                machine: Some(machine),
                inv: Inventory::new(),
                transition_funcs: Vec::new(),
            }),
            cv: Condvar::new(),
            disp,
        });

        let driver_hdl = Arc::clone(&this);
        let driver = thread::Builder::new()
            .name("instance-driver".to_string())
            .spawn(move || driver_hdl.drive_state())?;

        this.disp.finalize(&this, Some(vcpu_fn));
        let mut state = this.inner.lock().unwrap();
        state.drive_thread = Some(driver);
        drop(state);

        Ok(this)
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
        let mctx = MachineCtx::new(machine);
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
    pub fn set_target_state(&self, target: ReqState) -> Result<(), ()> {
        let mut inner = self.inner.lock().unwrap();

        if matches!(inner.state_target, Some(State::Halt | State::Destroy)) {
            // Cannot request any state once the target is halt/destroy
            return Err(());
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
        }
    }

    pub(crate) fn trigger_suspend(
        &self,
        kind: SuspendKind,
        source: SuspendSource,
    ) -> Result<(), ()> {
        let mut inner = self.inner.lock().unwrap();
        self.trigger_suspend_locked(&mut inner, kind, source)
    }

    fn trigger_suspend_locked(
        &self,
        inner: &mut MutexGuard<Inner>,
        kind: SuspendKind,
        source: SuspendSource,
    ) -> Result<(), ()> {
        if matches!(inner.state_current, State::Halt | State::Destroy) {
            // No way out from Halt or Destroy
            return Err(());
        }

        match kind {
            SuspendKind::Reset => {
                match inner.suspend_info {
                    Some((SuspendKind::Halt, _)) => {
                        // Cannot supersede active halt
                        return Err(());
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
    ) -> Result<(), ()> {
        if !State::valid_target(&inner.state_current, &target) {
            return Err(());
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
            // bail if we reach the target state _or Destroy
            state.state_current != target
                || state.state_current != State::Destroy
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
    ) {
        self.disp.with_ctx(|ctx| {
            // Allow any entity to act on the new state
            inner.inv.for_each_node(inventory::Order::Pre, |_id, rec| {
                rec.entity().state_transition(state, target, ctx);
            });

            for f in inner.transition_funcs.iter() {
                f(state, ctx)
            }
        });
    }

    fn drive_state(&self) {
        let mut next_state: Option<State> = None;
        let mut inner = self.inner.lock().unwrap();
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
            eprintln!(
                "Instance transition {:?} -> {:?} (target: {:?})",
                prev_state, state, &inner.state_target
            );
            if matches!(&inner.state_target, Some(s) if *s == state) {
                // target state has been reached
                inner.state_target = None;
            }

            // Pre-state-change actions
            match state {
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
                }
                _ => {}
            }

            self.transition_actions(&inner, state, inner.state_target);

            // Post-state-change actions
            match state {
                State::Boot => {
                    // A reset is as good as fulfilled when transitioning
                    // through the Boot state.
                    if inner.state_target == Some(State::Reset) {
                        inner.state_target = None;
                    }

                    if matches!(inner.state_target, None | Some(State::Run)) {
                        self.disp.release();
                    }
                }
                State::Destroy => {}
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
        let _joined = state.drive_thread.take().unwrap().join();
    }
}

#[cfg(test)]
impl Instance {
    pub(crate) fn new_test(rt_handle: Option<Handle>) -> io::Result<Arc<Self>> {
        let machine = Arc::new(Machine::new_test()?);
        let disp = Dispatcher::new(&machine, rt_handle);

        let this = Arc::new(Self {
            inner: Mutex::new(Inner {
                state_current: State::Initialize,
                state_target: None,
                suspend_info: None,
                drive_thread: None,
                machine: Some(machine),
                inv: Inventory::new(),
                transition_funcs: Vec::new(),
            }),
            cv: Condvar::new(),
            disp,
        });

        let driver_hdl = Arc::clone(&this);
        let driver = thread::Builder::new()
            .name("instance-driver".to_string())
            .spawn(move || driver_hdl.drive_state())?;

        // Start with no vCPU threads for now.
        this.disp.finalize(&this, None);
        let mut state = this.inner.lock().unwrap();
        state.drive_thread = Some(driver);
        drop(state);

        Ok(this)
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
