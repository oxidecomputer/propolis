use std::collections::BTreeMap;
use std::io::Result;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::{Builder, JoinHandle};

use super::{DispCtx, SharedCtx};

use tokio::runtime::Handle;

pub type WakeFn = dyn Fn(&DispCtx) + Send + 'static;
pub type SyncFn = dyn FnOnce(&mut SyncCtx) + Send + 'static;

#[derive(Copy, Clone, Eq, PartialEq, Ord, PartialOrd)]
pub struct SyncTaskId {
    val: usize,
}

struct Detail {
    #[allow(dead_code)]
    ident: String,

    join: Option<JoinHandle<()>>,
    ctrl: Arc<WorkerCtrl>,
    wake: Option<Box<WakeFn>>,
}

#[derive(Debug, Eq, PartialEq)]
enum State {
    Run,
    WaitQuiesce,
    WaitShutdown,
    Quiesce,
    Shutdown,
}

struct Inner {
    state: State,
    next_id: usize,
    workers: BTreeMap<SyncTaskId, Detail>,
}
impl Inner {
    fn next_id(&mut self) -> SyncTaskId {
        self.next_id += 1;
        SyncTaskId { val: self.next_id }
    }
}

pub(super) struct SyncDispatch {
    inner: Mutex<Inner>,
    cv: Condvar,
}

impl SyncDispatch {
    pub(super) fn new() -> Self {
        Self {
            inner: Mutex::new(Inner {
                next_id: 0,
                state: State::Quiesce,
                workers: BTreeMap::new(),
            }),
            cv: Condvar::new(),
        }
    }

    /// Spawns a new dedicated worker thread named `name` which invokes
    /// `func` on `data`.
    ///
    /// An optional `wake` function may be supplied, when invoked, this
    /// function should trigger the worker to move to a barrier point.
    pub fn spawn(
        &self,
        shared: SharedCtx,
        rt_hdl: Handle,
        name: String,
        func: Box<SyncFn>,
        wake: Option<Box<WakeFn>>,
    ) -> Result<()> {
        let mut inner = self.inner.lock().unwrap();
        let ctrl = match inner.state {
            State::Run => WorkerCtrl::create(),
            State::WaitQuiesce | State::Quiesce => WorkerCtrl::create_held(),
            State::WaitShutdown | State::Shutdown => {
                // As silly as this is, go ahead with worker creation, even
                // though we immediately request its exit.  That way the caller
                // does not need to handle a creation error.
                let ctrl = WorkerCtrl::create();
                ctrl.req_exit();
                ctrl
            }
        };
        let task_id = inner.next_id();
        let mut sctx = SyncCtx::for_worker(
            shared.log_child(slog::o!("sync_task" => name.clone())),
            Arc::clone(&ctrl),
        );
        let hdl = Builder::new().name(name.clone()).spawn(move || {
            if sctx.check_yield() {
                return;
            }
            // Ensure that worker thread can manipulate any tokio runtime
            // related state by entering said runtime
            let _rt_guard = rt_hdl.enter();

            func(&mut sctx);
        })?;

        let res = inner.workers.insert(
            task_id,
            Detail { ident: name, join: Some(hdl), ctrl, wake },
        );
        assert!(res.is_none());

        Ok(())
    }

    pub(super) fn release(&self) {
        let mut inner = self.inner.lock().unwrap();
        match inner.state {
            State::Run | State::WaitShutdown | State::Shutdown => {
                return;
            }
            State::WaitQuiesce => {
                inner = self
                    .cv
                    .wait_while(inner, |i| {
                        !matches!(i.state, State::Quiesce | State::Shutdown)
                    })
                    .unwrap();
                if inner.state == State::Shutdown {
                    return;
                }
            }
            State::Quiesce => {}
        }
        for (_id, worker) in inner.workers.iter() {
            worker.ctrl.release();
        }
        inner.state = State::Run;
    }

    fn push_to_barrier(
        shared: SharedCtx,
        inner: &MutexGuard<Inner>,
    ) -> Vec<Arc<WorkerCtrl>> {
        let mut sctx = SyncCtx { shared, ctrl: None };
        let ctx = sctx.dispctx();
        let mut ctrls = Vec::with_capacity(inner.workers.len());
        for (_id, wd) in inner.workers.iter() {
            let already_held = wd.ctrl.req_hold();

            if !already_held {
                if let Some(wake_fn) = wd.wake.as_ref() {
                    wake_fn(&ctx);
                }
            }
            ctrls.push(Arc::clone(&wd.ctrl));
        }
        ctrls
    }

    pub(super) fn quiesce(&self, shared: SharedCtx) {
        let mut inner = self.inner.lock().unwrap();
        match inner.state {
            State::Run => {
                inner.state = State::WaitQuiesce;
            }
            State::WaitQuiesce | State::WaitShutdown => {
                inner = self
                    .cv
                    .wait_while(inner, |i| {
                        !matches!(i.state, State::Quiesce | State::Shutdown)
                    })
                    .unwrap();
                drop(inner);
                return;
            }
            State::Quiesce | State::Shutdown => {
                return;
            }
        };

        let ctrls = Self::push_to_barrier(shared, &inner);

        // wait for all workers to report at their barriers.  This must be done
        // with `inner` unlocked, since those workers could be attempting a
        // spawn themselves
        assert_eq!(inner.state, State::WaitQuiesce);
        drop(inner);
        for ctrl in ctrls.iter() {
            ctrl.wait_until_held();
        }
        let mut inner = self.inner.lock().unwrap();
        if inner.state == State::WaitQuiesce {
            inner.state = State::Quiesce;
        }
        self.cv.notify_all();
    }

    pub(super) fn shutdown(&self, shared: SharedCtx) {
        let mut inner = self.inner.lock().unwrap();

        let ctrls = match inner.state {
            State::Shutdown => {
                return;
            }
            State::Run => {
                inner.state = State::WaitShutdown;
                Self::push_to_barrier(
                    shared
                        .log_child(slog::o!("dispatcher_action" => "shutdown")),
                    &inner,
                )
            }
            _ => {
                inner.state = State::WaitShutdown;
                let mut ctrls = Vec::with_capacity(inner.workers.len());
                for (_id, wd) in inner.workers.iter() {
                    ctrls.push(Arc::clone(&wd.ctrl));
                }
                ctrls
            }
        };
        drop(inner);

        // Signal all workers to exit, and wait for them to do so, or at least
        // reach the barrier point. This must be done with `inner` unlocked,
        // since those workers could be attempting a spawn themselves
        for wc in ctrls.iter() {
            wc.req_exit();
            wc.wait_until_held();
        }
        drop(ctrls);
        let mut inner = self.inner.lock().unwrap();
        assert!(matches!(inner.state, State::WaitShutdown | State::Shutdown));

        // Clean up and join all the threads
        inner.state = State::Shutdown;
        self.cv.notify_all();
        let mut workers = BTreeMap::new();
        std::mem::swap(&mut workers, &mut inner.workers);
        drop(inner);
        for (_id, wd) in workers.into_iter() {
            if let Some(join) = wd.join {
                let _ = join.join();
            }
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
    fn create() -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(WorkerCtrlState {
                req_hold: false,
                req_exit: false,
                exited: false,
                held: false,
            }),
            active_req: AtomicBool::new(false),
            cv: Condvar::new(),
        })
    }
    fn create_held() -> Arc<Self> {
        let this = Self::create();
        this.req_hold();
        this
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

    fn pending_reqs(&self) -> bool {
        if self.active_req.load(Ordering::Acquire) {
            let inner = self.inner.lock().unwrap();
            return inner.req_hold || inner.req_exit;
        }
        false
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

pub struct SyncCtx {
    shared: SharedCtx,
    ctrl: Option<Arc<WorkerCtrl>>,
}

impl SyncCtx {
    pub(super) fn standalone(shared: SharedCtx) -> Self {
        Self { shared, ctrl: None }
    }
    fn for_worker(shared: SharedCtx, ctrl: Arc<WorkerCtrl>) -> Self {
        Self { shared, ctrl: Some(ctrl) }
    }
    /// Returns true if the function holding this [`DispCtx`] object
    /// should yield control back to the dispatcher.
    ///
    /// If this thread has been requested to yield, this will block until the
    /// yield condition passes.
    pub fn check_yield(&mut self) -> bool {
        self.ctrl.as_ref().map_or(false, |c| c.check_yield())
    }

    /// Are there pending requests for this thread to yield or exit?
    pub fn pending_reqs(&self) -> bool {
        self.ctrl.as_ref().map_or(false, |c| c.pending_reqs())
    }

    /// Get access to `Logger` associated with this task
    pub fn log(&self) -> &slog::Logger {
        &self.shared.log
    }

    pub fn dispctx(&mut self) -> DispCtx {
        DispCtx {
            log: &self.shared.log,
            mctx: &self.shared.mctx,
            inst: &self.shared.inst,
            _async_permit: None,
        }
    }
}
impl Drop for SyncCtx {
    fn drop(&mut self) {
        if let Some(ctrl) = self.ctrl.as_ref() {
            ctrl.exit();
        }
    }
}
