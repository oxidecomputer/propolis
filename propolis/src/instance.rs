#![allow(unused)]

use std::io;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};

use crate::dispatch::*;
use crate::inventory::Inventory;
use crate::vcpu::VcpuRunFunc;
use crate::vmm::*;

#[derive(Copy, Clone, Eq, PartialEq, Debug)]
pub enum State {
    Initialize,
    Boot,
    Run,
    Quiesce,
    Halt,
    Reset,
    Destroy,
}
impl State {
    fn valid_target(from: &Self, to: &Self) -> bool {
        if from == to {
            return true;
        }
        match (from, to) {
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
}

type TransitionFunc = dyn Fn(State) + Send + Sync + 'static;

struct InnerState {
    current: State,
    target: Option<State>,
    drive_thread: Option<JoinHandle<()>>,
    machine: Arc<Machine>,
    disp: Dispatcher,
    inv: Inventory,
    transition_funcs: Vec<Box<TransitionFunc>>,
}
pub struct Instance {
    state: Mutex<InnerState>,
    cv: Condvar,
}
impl Instance {
    pub fn create(
        builder: Builder,
        vcpu_fn: VcpuRunFunc,
    ) -> io::Result<Arc<Self>> {
        let machine = Arc::new(builder.finalize()?);

        let mut disp = Dispatcher::create(&machine, vcpu_fn)?;
        let this = Arc::new(Self {
            state: Mutex::new(InnerState {
                current: State::Initialize,
                target: None,
                drive_thread: None,
                machine,
                disp,
                inv: Inventory::new(),
                transition_funcs: Vec::new(),
            }),
            cv: Condvar::new(),
        });

        let driver_hdl = Arc::clone(&this);
        let driver = thread::Builder::new()
            .name("instance-driver".to_string())
            .spawn(move || driver_hdl.drive_state())?;

        let mut state = this.state.lock().unwrap();
        state.drive_thread = Some(driver);
        state.disp.assoc_instance(Arc::downgrade(&this));
        drop(state);

        Ok(this)
    }

    pub fn initialize<F>(&self, func: F) -> io::Result<()>
    where
        F: FnOnce(
            &Machine,
            &MachineCtx,
            &Dispatcher,
            &Inventory,
        ) -> io::Result<()>,
    {
        let state = self.state.lock().unwrap();
        assert_eq!(state.current, State::Initialize);
        let mctx = MachineCtx::new(&state.machine);
        func(&state.machine, &mctx, &state.disp, &state.inv)
    }

    pub fn current_state(&self) -> State {
        let state = self.state.lock().unwrap();
        let res = state.current;
        drop(state);
        res
    }
    pub fn set_target_state(&self, target: State) -> io::Result<()> {
        let mut state = self.state.lock().unwrap();

        if !State::valid_target(&state.current, &target) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid target",
            ));
        }
        // XXX: verify validity of transitions
        state.target = Some(target);
        self.cv.notify_all();
        Ok(())
    }
    pub fn wait_for_state(&self, target: State) {
        let mut state = self.state.lock().unwrap();
        self.cv.wait_while(state, |state| {
            // bail if we reach the target state _or Destroy
            state.current != target || state.current != State::Destroy
        });
    }

    pub fn on_transition(&self, func: Box<TransitionFunc>) {
        let mut state = self.state.lock().unwrap();
        state.transition_funcs.push(func);
    }

    fn transition_cb(&self, state: &MutexGuard<InnerState>, next_state: State) {
        for f in state.transition_funcs.iter() {
            f(next_state)
        }
    }

    fn drive_state(&self) {
        let mut state = self.state.lock().unwrap();
        let mut next_state: Option<State> = None;
        loop {
            if let Some(next) = next_state.take() {
                // under going state transition
                state.current = next;
                self.cv.notify_all();

                match state.target.as_ref() {
                    Some(s) if *s == next => {
                        // target state has been reached
                        state.target = None;
                    }
                    _ => {}
                };
            } else {
                // waiting for next state target
                match state.target {
                    Some(t) => {
                        if t == state.current {
                            state.target = None;
                            continue;
                        }
                    }
                    None => {
                        state = self.cv.wait(state).unwrap();
                        continue;
                    }
                }
            }

            if let Some(t) = state.target.as_ref() {
                assert!(State::valid_target(&state.current, t));
            }
            let target = state.target.as_ref();

            let transition = match state.current {
                State::Initialize => match target {
                    Some(State::Destroy) => State::Destroy,
                    _ => State::Boot,
                },
                State::Boot => {
                    match target {
                        Some(State::Run) => {
                            // XXX: pause for conditions
                            state.disp.release_vcpus();
                            State::Run
                        }
                        _ => State::Quiesce,
                    }
                }
                State::Run => match target {
                    Some(_) => State::Quiesce,
                    None => State::Run,
                },
                State::Quiesce => {
                    state.disp.quiesce_workers();
                    match target {
                        Some(State::Halt) | Some(State::Destroy) => State::Halt,
                        Some(State::Reset) => State::Reset,
                        t => panic!("unexpected target {:?}", t),
                    }
                }
                State::Halt => {
                    // XXX: collect any data?
                    State::Destroy
                }
                State::Reset => {
                    // XXX: reset devices
                    State::Boot
                }
                State::Destroy => {
                    // XXX: clean up and bail
                    state.disp.destroy_workers();
                    self.transition_cb(&state, State::Destroy);
                    return;
                }
            };

            if transition != state.current {
                eprintln!(
                    "Instance transition {:?} -> {:?}",
                    state.current, transition
                );

                self.transition_cb(&state, transition);
                next_state = Some(transition);
            }
        }
    }

    pub fn print(&self) {
        let state = self.state.lock().unwrap();
        state.inv.print();
    }
}

impl Drop for Instance {
    fn drop(&mut self) {
        let mut state = self.state.lock().unwrap();
        if state.current != State::Destroy {
            drop(state);
            self.set_target_state(State::Destroy).unwrap();
            state = self.state.lock().unwrap();
            state = self
                .cv
                .wait_while(state, |state| state.current != State::Destroy)
                .unwrap();
        }
        let _joined = state.drive_thread.take().unwrap().join();
    }
}
