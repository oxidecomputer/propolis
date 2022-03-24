//! Implements utilities for dispatching operations to a virtual CPU.

use std::boxed::Box;
use std::io::Result;
use std::sync::{Arc, Weak};

use crate::instance::{self, Instance};
use crate::vmm::{Machine, MachineCtx};

use slog;
use tokio::runtime::Handle;
use tokio::task::JoinHandle;

mod async_tasks;
mod sync_tasks;
pub use async_tasks::*;
pub use sync_tasks::*;

/// Implements a VM-specific executor.
pub struct Dispatcher {
    async_disp: AsyncDispatch,
    sync_disp: SyncDispatch,
    inst: Weak<Instance>,
    // Getting access to the `Machine` via the `Instance` can be a problem
    // during `Instance::initialize` where locks may exclude us.  Keep an
    // independent reference to address that issue.
    machine: Weak<Machine>,
}

impl Dispatcher {
    /// Creates a new dispatcher.
    pub(crate) fn new(
        inst: Weak<Instance>,
        machine: Weak<Machine>,
        rt_handle: Option<Handle>,
    ) -> Arc<Self> {
        Arc::new(Self {
            async_disp: AsyncDispatch::new(rt_handle),
            sync_disp: SyncDispatch::new(),
            inst,
            machine,
        })
    }

    /// Spawns a new dedicated worker thread named `name` which invokes
    /// `func` on `data`.
    ///
    /// An optional `wake` function may be supplied, when invoked, this
    /// function should trigger the worker to move to a barrier point.
    pub fn spawn_sync(
        &self,
        name: String,
        func: Box<SyncFn>,
        wake: Option<Box<WakeFn>>,
    ) -> Result<()> {
        self.sync_disp.spawn(
            SharedCtx::from_disp(self),
            self.handle().unwrap(),
            name,
            func,
            wake,
        )
    }

    pub(crate) fn with_ctx(&self, func: impl FnOnce(&DispCtx)) {
        let mut sctx = SyncCtx::standalone(
            SharedCtx::from_disp(self)
                .log_child(slog::o!("dispatcher_action" => "ad-hoc")),
        );
        let ctx = sctx.dispctx();
        func(&ctx);
    }

    /// Quiesce tasks running under the dispatcher.  For sync threads, this
    /// means reaching their yield point (calling `sctx.check_yield()`).  For
    /// async tasks it means being without a `DispCtx` in scope from
    /// `actx.dispctx()`.  Tasks will be held outside these yield points until
    /// released or canceled.
    pub(crate) fn quiesce(&self) {
        self.sync_disp.quiesce(SharedCtx::from_disp(self));
        self.async_disp.quiesce_contexts();
    }

    /// Release tasks running in the dispatcher from their quiesce points.
    pub(crate) fn release(&self) {
        self.sync_disp.release();
        self.async_disp.release_contexts();
    }

    /// Release a specific task running in the dispatcher from its quiesce points.
    pub(crate) fn release_one(&self, id: CtxId) {
        self.async_disp.release_context(id);
    }

    /// Shutdown the dispatcher.  This will quiesce and stop all managed work
    /// (sync threads and async tasks).  New work cannot be started in the
    /// dispatcher after this point.
    pub(crate) fn shutdown(&self) {
        self.sync_disp.shutdown(SharedCtx::from_disp(self));
        self.async_disp.shutdown();
    }

    /// Get an `AsyncCtx`, useful for async tasks which require access to
    /// instance resources.
    pub fn async_ctx(&self) -> AsyncCtx {
        self.async_disp.context(SharedCtx::from_disp(self))
    }

    ///  Get access to the underlying tokio runtime handle
    pub fn handle(&self) -> Option<Handle> {
        self.async_disp.handle()
    }

    /// Track an async task so it is aborted when the dispatcher is shutdown
    pub fn track(&self, hdl: JoinHandle<()>) {
        self.async_disp.track(hdl);
    }
}

struct SharedCtx {
    mctx: MachineCtx,
    inst: Weak<Instance>,
    log: slog::Logger,
}
impl SharedCtx {
    fn from_disp(disp: &Dispatcher) -> Self {
        let inst = disp.inst.upgrade().unwrap();
        let log = inst.logger().clone();
        let mctx = MachineCtx::new(disp.machine.upgrade().unwrap());
        Self { mctx, inst: Arc::downgrade(&inst), log }
    }
    fn log_child<T: slog::SendSyncRefUnwindSafeKV + 'static>(
        mut self,
        param: slog::OwnedKV<T>,
    ) -> Self {
        self.log = self.log.new(param);
        self
    }
}

pub struct DispCtx<'a> {
    pub log: &'a slog::Logger,
    pub mctx: &'a MachineCtx,
    inst: &'a Weak<Instance>,
    _async_permit: Option<AsyncCtxPermit<'a>>,
}

impl<'a> DispCtx<'a> {
    /// Trigger a suspend (reboot or halt) of the instance.
    pub fn trigger_suspend(
        &self,
        kind: instance::SuspendKind,
        source: instance::SuspendSource,
    ) {
        let inst = Weak::upgrade(self.inst).unwrap();
        let _ = inst.trigger_suspend(kind, source);
    }

    /// Acquire an `AsyncCtx`, useful for accessing instance state from
    /// emulation running in an async runtime
    pub fn async_ctx(&self) -> AsyncCtx {
        let inst = Weak::upgrade(self.inst).unwrap();
        inst.async_ctx()
    }
}
