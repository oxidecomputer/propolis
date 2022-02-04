//! Implements utilities for dispatching operations to a virtual CPU.

use std::boxed::Box;
use std::io::Result;
use std::sync::{Arc, Weak};

use crate::common::ParentRef;
use crate::instance;
use crate::util::self_arc::*;
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
    machine: Arc<Machine>,
    parent: ParentRef<instance::Instance>,
    logger: slog::Logger,
    sa_cell: SelfArcCell<Self>,
}

impl Dispatcher {
    /// Creates a new dispatcher.
    ///
    /// # Arguments
    /// - `vm`: The machine for which the dispatcher will handle requests.
    /// - `vcpu_fn`: A function, which will be invoked by the dispatcher,
    /// to run the CPU. This function is responsible for yielding control
    /// back to the dispatcher when requested.
    pub(crate) fn new(
        vm: &Arc<Machine>,
        rt_handle: Option<Handle>,
        logger: slog::Logger,
    ) -> Arc<Self> {
        let mut this = Arc::new(Self {
            async_disp: AsyncDispatch::new(rt_handle),
            sync_disp: SyncDispatch::new(),
            machine: Arc::clone(vm),
            parent: ParentRef::new(),
            logger,
            sa_cell: SelfArcCell::new(),
        });
        SelfArc::self_arc_init(&mut this);
        this
    }

    pub fn logger(&self) -> &slog::Logger {
        &self.logger
    }

    /// Perform final setup tasks on the dispatcher, including spawning of
    /// threads for running the instance vCPUs.
    pub(crate) fn finalize(&self, inst: &Arc<instance::Instance>) {
        self.parent.set(inst);
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
        self.sync_disp.spawn(self, name, func, wake)
    }

    pub(crate) fn with_ctx(&self, func: impl FnOnce(&DispCtx)) {
        let mut sctx = SyncCtx::standalone(SharedCtx::child(
            self,
            slog::o!("dispatcher_action" => "ad-hoc"),
        ));
        let ctx = sctx.dispctx();
        func(&ctx);
    }

    /// Quiesce tasks running under the dispatcher.  For sync threads, this
    /// means reaching their yield point (calling `sctx.check_yield()`).  For
    /// async tasks it means being without a `DispCtx` in scope from
    /// `actx.dispctx()`.  Tasks will be held outside these yield points until
    /// released or canceled.
    pub(crate) fn quiesce(&self) {
        self.sync_disp.quiesce(self);
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
        self.sync_disp.shutdown(self);
        self.async_disp.shutdown();
    }

    /// Get an `AsyncCtx`, useful for async tasks which require access to
    /// instance resources.
    pub fn async_ctx(&self) -> AsyncCtx {
        self.async_disp.context(self)
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
impl SelfArc for Dispatcher {
    fn self_arc_cell(&self) -> &SelfArcCell<Self> {
        &self.sa_cell
    }
}

struct SharedCtx {
    mctx: MachineCtx,
    disp: Weak<Dispatcher>,
    inst: Weak<instance::Instance>,
    log: slog::Logger,
}
impl SharedCtx {
    fn create(disp: &Dispatcher) -> Self {
        Self {
            mctx: MachineCtx::new(&disp.machine),
            disp: disp.self_weak(),
            inst: disp.parent.get_weak(),
            log: disp.logger.clone(),
        }
    }
    fn child<T: slog::SendSyncRefUnwindSafeKV + 'static>(
        disp: &Dispatcher,
        param: slog::OwnedKV<T>,
    ) -> Self {
        Self {
            mctx: MachineCtx::new(&disp.machine),
            disp: disp.self_weak(),
            inst: disp.parent.get_weak(),
            log: disp.logger.new(param),
        }
    }
}

pub struct DispCtx<'a> {
    pub log: &'a slog::Logger,
    pub mctx: &'a MachineCtx,
    disp: &'a Weak<Dispatcher>,
    inst: &'a Weak<instance::Instance>,
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

    /// Spawn a sync worker task under the instance dispatcher
    pub fn spawn_sync(
        &self,
        name: String,
        func: Box<SyncFn>,
        wake: Option<Box<WakeFn>>,
    ) -> Result<()> {
        let disp = Weak::upgrade(&self.disp).unwrap();
        disp.spawn_sync(name, func, wake)
    }

    /// Acquire an `AsyncCtx`, useful for accessing instance state from
    /// emulation running in an async runtime
    pub fn async_ctx(&self) -> AsyncCtx {
        let disp = Weak::upgrade(&self.disp).unwrap();
        disp.async_ctx()
    }

    pub fn handle(&self) -> Option<Handle> {
        let disp = Weak::upgrade(&self.disp).unwrap();
        disp.handle()
    }
}
