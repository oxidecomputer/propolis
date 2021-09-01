//! Implements utilities for dispatching operations to a virtual CPU.

use std::collections::BTreeMap;
use std::io::Result;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, Weak};
use std::thread::{Builder, JoinHandle};

use crate::instance;
use crate::vcpu::*;
use crate::vmm::{Machine, MachineCtx};

pub mod event_ports;
pub mod events;

use events::{EventCtx, EventDispatch};

pub type WakeFn = dyn Fn(&DispCtx) + Send + 'static;

#[derive(Eq, PartialEq, Ord, PartialOrd)]
enum Worker {
    Vcpu(usize),
    Events,
    Custom(String),
}
struct WorkerState {
    join: Option<JoinHandle<()>>,
    ctrl: Arc<WorkerCtrl>,
    wake: Option<Box<WakeFn>>,
}

#[derive(Default)]
struct DispInner {
    inst: Mutex<Option<Weak<instance::Instance>>>,
}

/// Implements a VM-specific executor.
pub struct Dispatcher {
    mctx: MachineCtx,
    event_dispatch: Arc<EventDispatch>,
    workers: Mutex<BTreeMap<Worker, WorkerState>>,
    inner: Arc<DispInner>,
}

impl Dispatcher {
    /// Creates a new dispatcher.
    ///
    /// # Arguments
    /// - `vm`: The machine for which the dispatcher will handle requests.
    /// - `vcpu_fn`: A function, which will be invoked by the dispatcher,
    /// to run the CPU. This function is responsible for yielding control
    /// back to the dispatcher when requested.
    pub fn create(vm: &Arc<Machine>, vcpu_fn: VcpuRunFunc) -> Result<Self> {
        let mut workers = BTreeMap::new();
        let mctx = MachineCtx::new(vm);
        let event_dispatch = Arc::new(EventDispatch::new());
        let disp_inner = Arc::new(DispInner::default());

        // Spawn event dispatch thread.
        let evt_ctrl = WorkerCtrl::create(true);
        let mut evt_ctx = DispCtx::new(
            mctx.clone(),
            Arc::clone(&event_dispatch),
            Some(Arc::clone(&evt_ctrl)),
            Arc::clone(&disp_inner),
        );
        let evt_edisp = Arc::clone(&event_dispatch);
        let evt_join = Builder::new()
            .name("event-dispatch".to_string())
            .spawn(move || {
                if evt_ctx.check_yield() {
                    return;
                }
                events::event_loop(evt_edisp, &mut evt_ctx);
            })
            .unwrap();

        let wake_edisp = Arc::downgrade(&event_dispatch);
        workers.insert(
            Worker::Events,
            WorkerState {
                join: Some(evt_join),
                ctrl: evt_ctrl,
                wake: Some(Box::new(move |_ctx: &DispCtx| {
                    if let Some(edisp) = Weak::upgrade(&wake_edisp) {
                        edisp.notify();
                    }
                })),
            },
        );

        // Spawn vCPU threads
        for id in 0..mctx.max_cpus() {
            let ctrl = WorkerCtrl::create(true);
            let mut ctx = DispCtx::new(
                mctx.clone(),
                Arc::clone(&event_dispatch),
                Some(Arc::clone(&ctrl)),
                Arc::clone(&disp_inner),
            );
            let name = format!("vcpu-{}", id);
            let vcpu = VcpuHdl::new(vm.get_hdl(), id as i32);
            let hdl = Builder::new()
                .name(name)
                .spawn(move || {
                    // wait at dispatch hold point until start
                    if ctx.check_yield() {
                        return;
                    }

                    vcpu_fn(vcpu, &mut ctx);
                })
                .unwrap();

            workers.insert(
                Worker::Vcpu(id),
                WorkerState {
                    join: Some(hdl),
                    ctrl,
                    wake: Some(Box::new(move |ctx: &DispCtx| {
                        let _ = ctx.mctx.vcpu(id).barrier();
                    })),
                },
            );
        }

        // Release the events thread to run immediately
        workers.get(&Worker::Events).unwrap().ctrl.release();

        let this = Self {
            mctx,
            event_dispatch,
            workers: Mutex::new(workers),
            inner: disp_inner,
        };
        Ok(this)
    }

    /// Associates an instance with a dispatcher.
    ///
    /// # Panics
    ///
    /// Panics if the dispatcher has already been associated with
    /// an instance.
    pub(crate) fn assoc_instance(&self, inst: Weak<instance::Instance>) {
        let res = self.inner.inst.lock().unwrap().replace(inst);
        assert!(res.is_none());
    }

    /// Spawns a new dedicated worker thread named `name` which invokes
    /// `func` on `data`.
    ///
    /// An optional `wake` function may be supplied, when invoked, this
    /// function should trigger the worker to move to a barrier point.
    pub fn spawn<D>(
        &self,
        name: String,
        data: D,
        func: fn(D, &mut DispCtx),
        wake: Option<Box<WakeFn>>,
    ) -> Result<()>
    where
        D: Send + 'static,
    {
        let ctrl = WorkerCtrl::create(false);
        let mut ctx = DispCtx::new(
            self.mctx.clone(),
            self.event_dispatch.clone(),
            Some(Arc::clone(&ctrl)),
            Arc::clone(&self.inner),
        );
        let hdl = Builder::new().name(name.clone()).spawn(move || {
            func(data, &mut ctx);
        })?;

        let mut workers = self.workers.lock().unwrap();
        let res = workers.insert(
            Worker::Custom(name),
            WorkerState { join: Some(hdl), ctrl, wake },
        );
        assert!(res.is_none());

        Ok(())
    }

    /// Waits for the termination of all workers.
    pub fn join(&self) {
        // XXX: indicate to all threads that they should bail out
        let mut workers = self.workers.lock().unwrap();

        for (_id, ws) in workers.iter_mut() {
            let hdl = ws.join.take().unwrap();
            hdl.join().unwrap()
        }
    }

    /// Creates a new context object which provides an
    /// API of a subset of the Dispatcher.
    pub fn ctx(&self) -> DispCtx {
        let ctrl = WorkerCtrl::create(false);
        DispCtx::new(
            self.mctx.clone(),
            self.event_dispatch.clone(),
            Some(ctrl),
            Arc::clone(&self.inner),
        )
    }

    pub(crate) fn release_workers(&self) {
        let workers = self.workers.lock().unwrap();
        for (_id, worker) in workers.iter() {
            worker.ctrl.release();
        }
    }

    pub(crate) fn quiesce_workers(&self) {
        let workers = self.workers.lock().unwrap();
        let ctx = DispCtx::new(
            self.mctx.clone(),
            self.event_dispatch.clone(),
            None,
            Arc::clone(&self.inner),
        );

        for (_id, ws) in workers.iter() {
            let already_held = ws.ctrl.req_hold();

            if !already_held {
                if let Some(wake_fn) = ws.wake.as_ref() {
                    wake_fn(&ctx);
                }
            }
        }

        // wait for all workers to report at their barriers
        for (_id, ws) in workers.iter() {
            ws.ctrl.wait_until_held();
        }
    }

    pub(crate) fn destroy_workers(&self) {
        let workers = self.workers.lock().unwrap();
        let ctx = DispCtx::new(
            self.mctx.clone(),
            self.event_dispatch.clone(),
            None,
            Arc::clone(&self.inner),
        );

        // push all workers to their barrier points
        for (_id, ws) in workers.iter() {
            let already_held = ws.ctrl.req_hold();

            if !already_held {
                if let Some(wake_fn) = ws.wake.as_ref() {
                    wake_fn(&ctx);
                }
            }
        }

        for (_id, ws) in workers.iter() {
            ws.ctrl.wait_until_held();
            ws.ctrl.req_exit();
        }
    }
}

struct WorkerCtrlState {
    req_hold: bool,
    req_exit: bool,
    exited: bool,
    held: bool,
}
struct WorkerCtrl {
    inner: Mutex<WorkerCtrlState>,
    active_req: AtomicBool,
    cv: Condvar,
}
impl WorkerCtrl {
    fn create(initial_hold: bool) -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(WorkerCtrlState {
                req_hold: initial_hold,
                req_exit: false,
                exited: false,
                held: false,
            }),
            active_req: AtomicBool::new(initial_hold),
            cv: Condvar::new(),
        })
    }
    fn req_hold(&self) -> bool {
        let mut inner = self.inner.lock().unwrap();
        inner.req_hold = true;
        self.active_req.store(true, Ordering::Release);

        // Is this thread already waiting at hold point?
        let held = inner.held;
        drop(inner);
        held
    }
    fn req_exit(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.req_exit = true;
        self.active_req.store(true, Ordering::Release);

        // If the worker is waiting at the hold point, notify it so it can wake
        // and proceed in an orderly fashion to the exit(s).
        if inner.held {
            self.cv.notify_all();
        }
    }
    fn release(&self) {
        let mut inner = self.inner.lock().unwrap();
        self.active_req.store(true, Ordering::Release);
        inner.req_hold = false;
        if inner.held {
            self.cv.notify_all();
        }
    }
    fn reconcile_reqs(&self, inner: &mut MutexGuard<WorkerCtrlState>) {
        self.active_req
            .store(inner.req_hold | inner.req_exit, Ordering::Release);
    }
    fn exit(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.exited = true;
    }

    fn check_yield(&self) -> bool {
        if self.active_req.load(Ordering::Acquire) {
            let mut inner = self.inner.lock().unwrap();
            if inner.req_hold {
                inner.held = true;
                // notify that we're held
                self.cv.notify_all();
                inner = self
                    .cv
                    .wait_while(inner, |inner| {
                        inner.req_hold && !inner.req_exit
                    })
                    .unwrap();
                inner.held = false;
                self.reconcile_reqs(&mut inner);
            }
            let should_exit = inner.req_exit;
            drop(inner);
            should_exit
        } else {
            false
        }
    }

    fn wait_until_held(&self) {
        let inner = self.inner.lock().unwrap();
        assert!(inner.req_hold);
        if !inner.held && !inner.exited {
            let _guard = self
                .cv
                .wait_while(inner, |inner| !inner.held && !inner.exited)
                .unwrap();
        }
    }
}

/// Context to methods invoked from [`Dispatcher`].
pub struct DispCtx {
    pub mctx: MachineCtx,
    pub event: EventCtx,
    ctrl: Option<Arc<WorkerCtrl>>,
    di: Arc<DispInner>,
}

impl DispCtx {
    fn new(
        mctx: MachineCtx,
        edisp: Arc<EventDispatch>,
        ctrl: Option<Arc<WorkerCtrl>>,
        inner: Arc<DispInner>,
    ) -> DispCtx {
        DispCtx { mctx, event: EventCtx::new(edisp), ctrl, di: inner }
    }

    /// Returns true if the function holding this [`DispCtx`] object
    /// should yield control back to the dispatcher.
    pub fn check_yield(&mut self) -> bool {
        if let Some(ctrl) = self.ctrl.as_ref() {
            ctrl.check_yield()
        } else {
            false
        }
    }

    pub fn trigger_suspend(
        &self,
        kind: instance::SuspendKind,
        source: instance::SuspendSource,
    ) {
        let guard = self.di.inst.lock().unwrap();
        let inst = Weak::upgrade(&guard.as_ref().unwrap()).unwrap();
        let _ = inst.trigger_suspend(kind, source);
    }
}
impl Drop for DispCtx {
    fn drop(&mut self) {
        if let Some(ctrl) = self.ctrl.as_ref() {
            ctrl.exit();
        }
    }
}
