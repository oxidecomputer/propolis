use std::collections::hash_map::HashMap;
use std::sync::{Arc, Mutex};

use futures::future::BoxFuture;
use tokio::runtime::{Builder, Runtime};
use tokio::sync::Semaphore;
use tokio::task::JoinHandle;

use super::{DispCtx, SharedCtx};

#[derive(Copy, Clone, Eq, PartialEq, Hash)]
pub struct AsyncTaskId {
    val: usize,
}

struct AsyncTask {
    hdl: JoinHandle<()>,
    ctx_sem: Arc<Semaphore>,
    task_sem: Arc<Semaphore>,
    quiesced: bool,
}

#[derive(Default)]
struct AsyncInner {
    next_id: usize,
    quiesced: bool,
    shutdown: bool,
    tasks: HashMap<AsyncTaskId, AsyncTask>,
}
impl AsyncInner {
    fn next_id(&mut self) -> AsyncTaskId {
        self.next_id += 1;
        AsyncTaskId { val: self.next_id }
    }
}

pub(super) struct AsyncDispatch {
    inner: Mutex<AsyncInner>,
    rt: Mutex<Option<Runtime>>,
}
impl AsyncDispatch {
    pub(super) fn new() -> Self {
        let runtime = Builder::new_multi_thread().enable_all().build().unwrap();
        Self { inner: Default::default(), rt: Mutex::new(Some(runtime)) }
    }
    pub(super) fn spawn(
        &self,
        shared: SharedCtx,
        taskfn: impl FnOnce(AsyncCtx) -> BoxFuture<'static, ()>,
    ) -> AsyncTaskId {
        let mut inner = self.inner.lock().unwrap();

        assert!(!inner.shutdown);

        let id = inner.next_id();
        let task_sem = Arc::new(Semaphore::new(1));
        let (actx, ctx_sem) = AsyncCtx::new(shared);
        let moved_task_sem = Arc::clone(&task_sem);

        let task = taskfn(actx);

        let rt = self.rt.lock().unwrap();
        let hdl = rt.as_ref().unwrap().spawn(async {
            if let Ok(_guard) = Semaphore::acquire_owned(moved_task_sem).await {
                task.await;
            }
        });

        if !inner.quiesced {
            // Make the async ctx available if tasks are not quiesced
            ctx_sem.add_permits(1);
        }
        let task =
            AsyncTask { hdl, ctx_sem, task_sem, quiesced: inner.quiesced };
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
                    let permit = task.ctx_sem.acquire().await.unwrap();
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
        for (_id, task) in inner.tasks.iter_mut() {
            if task.quiesced {
                task.ctx_sem.add_permits(1);
                task.quiesced = false;
            }
        }
    }
    pub(super) fn cancel(&self, id: AsyncTaskId) {
        let mut inner = self.inner.lock().unwrap();
        if let Some(task) = inner.tasks.remove(&id) {
            let rt = self.rt.lock().unwrap();
            rt.as_ref().unwrap().block_on(async {
                // Abort the task if it has not already finished
                if let Err(_) = task.task_sem.try_acquire() {
                    task.hdl.abort();
                }
                let _ = task.task_sem.acquire().await;
            });
        }
    }
    pub(super) fn shutdown(&self) {
        let mut inner = self.inner.lock().unwrap();
        if inner.shutdown {
            return;
        }
        inner.shutdown = true;
        let mut tasks = HashMap::new();
        std::mem::swap(&mut inner.tasks, &mut tasks);
        drop(inner);

        let mut guard = self.rt.lock().unwrap();
        let rt = guard.as_ref().unwrap();
        rt.block_on(async {
            for (_id, task) in tasks.iter() {
                task.hdl.abort();
            }
            // wait for all tasks to finish or cancel
            for task in tasks.into_values() {
                let _ = Semaphore::acquire_owned(task.task_sem).await;
            }
        });
        guard.take().unwrap().shutdown_background();
    }
}

pub struct AsyncCtx {
    shared: SharedCtx,
    ctrl: Arc<Semaphore>,
}
impl AsyncCtx {
    fn new(shared: SharedCtx) -> (Self, Arc<Semaphore>) {
        let sem = Arc::new(Semaphore::new(0));
        let this = Self { shared, ctrl: Arc::clone(&sem) };
        (this, sem)
    }
    pub async fn dispctx(&mut self) -> Option<DispCtx<'_>> {
        let permit = self.ctrl.acquire().await.ok()?;

        Some(DispCtx {
            mctx: &self.shared.mctx,
            disp: &self.shared.disp,
            inst: &self.shared.inst,
            _async_permit: Some(permit),
        })
    }
}
