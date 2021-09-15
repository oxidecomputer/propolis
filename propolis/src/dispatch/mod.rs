//! Implements utilities for dispatching operations to a virtual CPU.

use std::boxed::Box;
use std::io::Result;
use std::sync::{Arc, Weak};

use crate::common::ParentRef;
use crate::instance;
use crate::util::self_arc::*;
use crate::vcpu::*;
use crate::vmm::{Machine, MachineCtx};

use futures::future::BoxFuture;
use tokio::sync::SemaphorePermit;

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
    pub fn new(vm: &Arc<Machine>) -> Arc<Self> {
        let mut this = Arc::new(Self {
            async_disp: AsyncDispatch::new(),
            sync_disp: SyncDispatch::new(),
            machine: Arc::clone(vm),
            parent: ParentRef::new(),
            sa_cell: SelfArcCell::new(),
        });
        SelfArc::self_arc_init(&mut this);
        this
    }

    /// Perform final setup tasks on the dispatcher, including spawning of
    /// threads for running the instance vCPUs.
    pub(crate) fn finalize(
        &self,
        inst: &Arc<instance::Instance>,
        vcpu_fn: VcpuRunFunc,
    ) {
        self.parent.set(inst);
        let mctx = MachineCtx::new(&self.machine);
        for vcpu in mctx.vcpus() {
            let shared = SharedCtx::create(self);
            self.sync_disp.spawn_vcpu(shared, vcpu, vcpu_fn);
        }
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
        self.sync_disp.spawn(name, func, wake, SharedCtx::create(self))
    }

    pub(crate) fn with_ctx(&self, func: impl FnOnce(&DispCtx)) {
        let mut sctx = SyncCtx::standalone(SharedCtx::create(self));
        let ctx = sctx.dispctx();
        func(&ctx);
    }

    /// Quiesce tasks running under the dispatcher.  For sync threads, this
    /// means reaching their yield point (calling `sctx.check_yield()`).  For
    /// async tasks it means being without a `DispCtx` in scope from
    /// `actx.dispctx()`.  Tasks will be held outside these yield points until
    /// released or canceled.
    pub(crate) fn quiesce(&self) {
        self.sync_disp.quiesce(SharedCtx::create(self));
        self.async_disp.quiesce_tasks();
    }

    /// Release tasks running in the dispatcher from their quiesce points.
    pub(crate) fn release(&self) {
        self.sync_disp.release();
        self.async_disp.release_tasks();
    }

    /// Shutdown the dispatcher.  This will quiesce and stop all managed work
    /// (sync threads and async tasks).  New work cannot be started in the
    /// dispatcher after this point.
    pub(crate) fn shutdown(&self) {
        self.sync_disp.shutdown(SharedCtx::create(self));
        self.async_disp.shutdown();
    }

    /// Spawn an async tasks in the dispatcher.
    pub fn spawn_async(
        &self,
        task: impl FnOnce(AsyncCtx) -> BoxFuture<'static, ()>,
    ) -> AsyncTaskId {
        self.async_disp.spawn(SharedCtx::create(self), task)
    }

    /// Cancel an async task running under the dispatcher
    pub fn cancel_async(&self, id: AsyncTaskId) {
        self.async_disp.cancel(id);
    }

    pub async fn wait_exited(&self, id: AsyncTaskId) {
        self.async_disp.wait_exited(id).await;
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
}
impl SharedCtx {
    fn create(disp: &Dispatcher) -> Self {
        Self {
            mctx: MachineCtx::new(&disp.machine),
            disp: disp.self_weak(),
            inst: disp.parent.get_weak(),
        }
    }
}

pub struct DispCtx<'a> {
    pub mctx: &'a MachineCtx,
    disp: &'a Weak<Dispatcher>,
    inst: &'a Weak<instance::Instance>,
    _async_permit: Option<SemaphorePermit<'a>>,
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

    /// Spawn an async task under the instance dispatcher
    pub fn spawn_async(
        &self,
        task: impl FnOnce(AsyncCtx) -> BoxFuture<'static, ()>,
    ) -> AsyncTaskId {
        let disp = Weak::upgrade(&self.disp).unwrap();
        disp.spawn_async(task)
    }

    /// Cancel an async task running under the instance dispatcher.
    ///
    /// Returns when the task has been cancelled or exited on its own.  A task
    /// must not attempt to cancel itself.
    pub fn cancel_async(&self, id: AsyncTaskId) {
        let disp = Weak::upgrade(&self.disp).unwrap();
        disp.cancel_async(id);
    }
}
