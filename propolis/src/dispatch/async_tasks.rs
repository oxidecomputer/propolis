use std::collections::hash_map::HashMap;
use std::io::Result;
use std::sync::{Arc, Mutex, Weak};

use futures::future::BoxFuture;
use tokio::runtime::{Builder, Runtime};
use tokio::sync::mpsc;
use tokio::sync::Semaphore;
use tokio::task::JoinHandle;

use super::{DispCtx, SharedCtx, SyncFn, WakeFn};

#[derive(Copy, Clone, Eq, PartialEq, Hash)]
pub struct AsyncTaskId {
    val: usize,
}

struct TaskLife {
    run_sem: Arc<Semaphore>,
    id: AsyncTaskId,
    chan: mpsc::UnboundedSender<AsyncTaskId>,
}
impl Drop for TaskLife {
    fn drop(&mut self) {
        let _ = self.chan.send(self.id);
    }
}

struct TaskCtrl {
    ctx_guard: Semaphore,
    quiesce_notify: Semaphore,
}

struct AsyncTask {
    hdl: JoinHandle<()>,
    ctrl: Arc<TaskCtrl>,
    run_sem: Arc<Semaphore>,
    quiesced: bool,
}

struct AsyncInner {
    next_id: usize,
    quiesced: bool,
    shutdown: bool,
    tasks: HashMap<AsyncTaskId, AsyncTask>,
    life_sender: mpsc::UnboundedSender<AsyncTaskId>,
}
impl AsyncInner {
    fn next_id(&mut self) -> AsyncTaskId {
        self.next_id += 1;
        AsyncTaskId { val: self.next_id }
    }
    fn create() -> (Arc<Mutex<Self>>, mpsc::UnboundedReceiver<AsyncTaskId>) {
        let (life_sender, life_receiver) = mpsc::unbounded_channel();
        let this = Self {
            next_id: 0,
            quiesced: false,
            shutdown: false,
            tasks: HashMap::new(),
            life_sender,
        };
        (Arc::new(Mutex::new(this)), life_receiver)
    }
    fn prune(&mut self, id: AsyncTaskId) {
        let _ = self.tasks.remove(&id);
    }
    async fn wait_gone(inner: &Mutex<Self>, id: AsyncTaskId) {
        // TODO: In theory, if the runtime takes its time to actually execute
        // the future associated with this task, it may not proceed to acquiring
        // its permit from the run semaphore before some other actor attempts to
        // `wait_exited` it, causing this loop to spin tightly.  It is not a
        // large concern for now.
        loop {
            let sem = {
                let guard = inner.lock().unwrap();
                if let Some(task) = guard.tasks.get(&id) {
                    Arc::clone(&task.run_sem)
                } else {
                    return;
                }
            };
            let _ = sem.acquire().await;
        }
    }
}

pub(super) struct AsyncDispatch {
    inner: Arc<Mutex<AsyncInner>>,
    rt: Mutex<Option<Runtime>>,
}
impl AsyncDispatch {
    pub(super) fn new() -> Self {
        let (inner, mut receiver) = AsyncInner::create();
        let runtime = Builder::new_multi_thread().enable_all().build().unwrap();

        // Spawn worker to prune tasks from the dispatcher state when they are
        // finished or cancelled.
        let inner_prune = Arc::clone(&inner);
        runtime.spawn(async move {
            while let Some(id) = receiver.recv().await {
                let mut inner = inner_prune.lock().unwrap();
                inner.prune(id);
            }
        });

        Self { inner, rt: Mutex::new(Some(runtime)) }
    }
    pub(super) fn spawn(
        &self,
        shared: SharedCtx,
        taskfn: impl FnOnce(AsyncCtx) -> BoxFuture<'static, ()>,
    ) -> AsyncTaskId {
        let mut inner = self.inner.lock().unwrap();

        assert!(!inner.shutdown);

        let id = inner.next_id();
        let run_sem = Arc::new(Semaphore::new(1));
        let ctrl = Arc::new(TaskCtrl {
            ctx_guard: Semaphore::new(0),
            quiesce_notify: Semaphore::new(0),
        });
        let life = TaskLife {
            run_sem: Arc::clone(&run_sem),
            chan: inner.life_sender.clone(),
            id,
        };
        let actx = AsyncCtx { shared, ctrl: Arc::clone(&ctrl) };

        let task_fut = taskfn(actx);

        let rt = self.rt.lock().unwrap();
        let hdl = rt.as_ref().unwrap().spawn(async move {
            if let Ok(_guard) = life.run_sem.acquire().await {
                task_fut.await;
            }
        });

        if !inner.quiesced {
            // Make the async ctx available if tasks are not quiesced
            ctrl.ctx_guard.add_permits(1);
        } else {
            // Notify task that the ctx is in a quiesced state
            ctrl.quiesce_notify.add_permits(1);
        }
        let task = AsyncTask { hdl, ctrl, run_sem, quiesced: inner.quiesced };
        inner.tasks.insert(id, task);

        id
    }
    pub(super) fn quiesce_tasks(&self) {
        let mut inner = self.inner.lock().unwrap();
        if inner.quiesced {
            return;
        }
        inner.quiesced = true;

        let rt = self.rt.lock().unwrap();
        rt.as_ref().unwrap().block_on(async {
            for (_id, task) in inner.tasks.iter_mut() {
                if !task.quiesced {
                    task.quiesced = true;
                    task.ctrl.quiesce_notify.add_permits(1);
                    let permit = task.ctrl.ctx_guard.acquire().await.unwrap();
                    permit.forget();
                }
            }
        });
    }
    pub(super) fn release_tasks(&self) {
        let mut inner = self.inner.lock().unwrap();
        if !inner.quiesced {
            return;
        }
        inner.quiesced = false;

        let rt = self.rt.lock().unwrap();
        rt.as_ref().unwrap().block_on(async {
            for (_id, task) in inner.tasks.iter_mut() {
                if task.quiesced {
                    let permit =
                        task.ctrl.quiesce_notify.acquire().await.unwrap();
                    permit.forget();
                    task.ctrl.ctx_guard.add_permits(1);
                    task.quiesced = false;
                }
            }
        });
    }
    /// If a task is still registered with the dispatcher, cancel it.
    pub(super) fn cancel(&self, id: AsyncTaskId) -> bool {
        let inner = self.inner.lock().unwrap();
        if let Some(task) = inner.tasks.get(&id) {
            task.hdl.abort();
            false
        } else {
            true
        }
    }

    pub(super) async fn wait_exited(&self, id: AsyncTaskId) {
        AsyncInner::wait_gone(&self.inner, id).await;
    }

    pub(super) fn shutdown(&self) {
        let mut inner = self.inner.lock().unwrap();
        if inner.shutdown {
            return;
        }
        for (_id, task) in inner.tasks.iter() {
            task.hdl.abort();
        }
        inner.shutdown = true;
        drop(inner);

        let mut guard = self.rt.lock().unwrap();
        guard.take().unwrap().shutdown_background();
    }
}

pub struct AsyncCtx {
    shared: SharedCtx,
    ctrl: Arc<TaskCtrl>,
}
impl AsyncCtx {
    /// Get access to the `DispCtx` associated with this task.  This will not be
    /// immediately ready if the task is quiesced.  It will fail with a `None`
    /// value if the task is being cancelled or the containing Dispatcher is
    /// being shutdown.
    pub async fn dispctx(&mut self) -> Option<DispCtx<'_>> {
        let permit = self.ctrl.ctx_guard.acquire().await.ok()?;

        Some(DispCtx {
            mctx: &self.shared.mctx,
            disp: &self.shared.disp,
            inst: &self.shared.inst,
            _async_permit: Some(permit),
        })
    }

    /// Wait for this task to receive a quiesce request.  This is primarily
    /// intended to provide wake-ups in situations where forward progress would
    /// otherwise be impossible when the instance is being quiesced.
    pub async fn quiesce_requested(&self) {
        let _permit = self.ctrl.quiesce_notify.acquire().await.unwrap();
    }

    /// Spawn a sync worker task under the instance dispatcher
    pub fn spawn_sync(
        &self,
        name: String,
        func: Box<SyncFn>,
        wake: Option<Box<WakeFn>>,
    ) -> Result<()> {
        let disp = Weak::upgrade(&self.shared.disp).unwrap();
        disp.spawn_sync(name, func, wake)
    }

    /// Spawn an async task under the instance dispatcher
    pub fn spawn_async(
        &self,
        task: impl FnOnce(AsyncCtx) -> BoxFuture<'static, ()>,
    ) -> AsyncTaskId {
        let disp = Weak::upgrade(&self.shared.disp).unwrap();
        disp.spawn_async(task)
    }

    /// Cancel an async task running under the instance dispatcher.
    ///
    /// Returns when the task has been cancelled or exited on its own.  A task
    /// must not attempt to cancel itself.
    pub fn cancel_async(&self, id: AsyncTaskId) {
        let disp = Weak::upgrade(&self.shared.disp).unwrap();
        disp.cancel_async(id);
    }

    pub async fn wait_exited(&self, id: AsyncTaskId) {
        let disp = Weak::upgrade(&self.shared.disp).unwrap();
        disp.wait_exited(id).await;
    }
}
