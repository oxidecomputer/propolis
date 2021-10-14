use std::collections::hash_map::HashMap;
use std::future::Future;
use std::io::Result;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak};

use tokio::runtime::{Builder, Handle, Runtime};
use tokio::sync::mpsc;
use tokio::sync::Semaphore;
use tokio::task::JoinHandle;

use super::{DispCtx, Dispatcher, SharedCtx, SyncFn, WakeFn};

#[derive(Copy, Clone, Eq, PartialEq, Hash)]
pub struct AsyncTaskId {
    val: usize,
}
impl slog::Value for AsyncTaskId {
    fn serialize(
        &self,
        _record: &slog::Record,
        key: slog::Key,
        serializer: &mut dyn slog::Serializer,
    ) -> slog::Result {
        serializer.emit_usize(key, self.val)
    }
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
    quiesce_guard: Semaphore,
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

enum RuntimeBacking {
    Owned(Runtime),
    External(Handle),
}

pub(super) struct AsyncDispatch {
    inner: Arc<Mutex<AsyncInner>>,
    backing: Mutex<Option<RuntimeBacking>>,
}
impl AsyncDispatch {
    pub(super) fn new(rt_handle: Option<Handle>) -> Self {
        let (inner, mut receiver) = AsyncInner::create();

        let (backing, handle) = match rt_handle {
            Some(handle) => {
                let hc = handle.clone();
                (RuntimeBacking::External(handle), hc)
            }
            None => {
                let rt =
                    Builder::new_multi_thread().enable_all().build().unwrap();
                let handle = rt.handle().clone();
                (RuntimeBacking::Owned(rt), handle)
            }
        };

        // Spawn worker to prune tasks from the dispatcher state when they are
        // finished or cancelled.
        let inner_prune = Arc::clone(&inner);
        handle.spawn(async move {
            while let Some(id) = receiver.recv().await {
                let mut inner = inner_prune.lock().unwrap();
                inner.prune(id);
            }
        });

        Self { inner, backing: Mutex::new(Some(backing)) }
    }
    pub(super) fn spawn<T, F>(
        &self,
        disp: &Dispatcher,
        taskfn: T,
    ) -> AsyncTaskId
    where
        T: FnOnce(AsyncCtx) -> F,
        F: Future<Output = ()> + Send + 'static,
    {
        let mut inner = self.inner.lock().unwrap();

        assert!(!inner.shutdown);

        let id = inner.next_id();
        let run_sem = Arc::new(Semaphore::new(1));
        let ctrl = Arc::new(TaskCtrl {
            quiesce_guard: Semaphore::new(0),
            quiesce_notify: Semaphore::new(0),
        });
        let life = TaskLife {
            run_sem: Arc::clone(&run_sem),
            chan: inner.life_sender.clone(),
            id,
        };
        let actx = AsyncCtx::new(
            SharedCtx::child(disp, slog::o!("async_task" => id)),
            Arc::clone(&ctrl),
        );

        let task_fut = Box::pin(taskfn(actx));

        let hdl = self.with_handle(|hdl| {
            hdl.spawn(async move {
                if let Ok(_guard) = life.run_sem.acquire().await {
                    task_fut.await;
                }
            })
        });
        // let rt = self.backing.lock().unwrap();
        // let hdl = rt.as_ref().unwrap().spawn(async move {
        //     if let Ok(_guard) = life.run_sem.acquire().await {
        //         task_fut.await;
        //     }
        // });

        if !inner.quiesced {
            // Make the async ctx available if tasks are not quiesced
            ctrl.quiesce_guard.add_permits(1);
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

        self.with_handle(|hdl| {
            hdl.block_on(async {
                for (_id, task) in inner.tasks.iter_mut() {
                    if !task.quiesced {
                        task.quiesced = true;
                        task.ctrl.quiesce_notify.add_permits(1);
                        let permit =
                            task.ctrl.quiesce_guard.acquire().await.unwrap();
                        permit.forget();
                    }
                }
            })
        });
    }
    pub(super) fn release_tasks(&self) {
        let mut inner = self.inner.lock().unwrap();
        if !inner.quiesced {
            return;
        }
        inner.quiesced = false;

        self.with_handle(|hdl| {
            hdl.block_on(async {
                for (_id, task) in inner.tasks.iter_mut() {
                    if task.quiesced {
                        let permit =
                            task.ctrl.quiesce_notify.acquire().await.unwrap();
                        permit.forget();
                        task.ctrl.quiesce_guard.add_permits(1);
                        task.quiesced = false;
                    }
                }
            })
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

        let mut guard = self.backing.lock().unwrap();
        match guard.take().unwrap() {
            RuntimeBacking::Owned(rt) => rt.shutdown_background(),
            RuntimeBacking::External(_) => {}
        }
    }

    fn with_handle<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&Handle) -> R,
    {
        let guard = self.backing.lock().unwrap();
        let backing = guard.as_ref().unwrap();
        match backing {
            RuntimeBacking::Owned(rt) => f(rt.handle()),
            RuntimeBacking::External(h) => f(h),
        }
    }
}

pub struct AsyncCtx {
    shared: SharedCtx,
    ctrl: Arc<TaskCtrl>,
    ctx_live: Semaphore,
    ctx_count: AtomicUsize,
}
impl AsyncCtx {
    fn new(shared: SharedCtx, ctrl: Arc<TaskCtrl>) -> Self {
        Self {
            shared,
            ctrl,
            ctx_live: Semaphore::new(0),
            ctx_count: AtomicUsize::new(0),
        }
    }

    /// Get access to the `DispCtx` associated with this task.  This will not be
    /// immediately ready if the task is quiesced.  It will fail with a `None`
    /// value if the task is being cancelled or the containing Dispatcher is
    /// being shutdown.
    pub async fn dispctx(&self) -> Option<DispCtx<'_>> {
        let permit = self.acquire_permit().await?;
        Some(DispCtx {
            log: &self.shared.log,
            mctx: &self.shared.mctx,
            disp: &self.shared.disp,
            inst: &self.shared.inst,
            _async_permit: Some(permit),
        })
    }

    /// Get access to `Logger` associated with this task
    pub fn log(&self) -> &slog::Logger {
        &self.shared.log
    }

    async fn acquire_permit(&self) -> Option<AsyncCtxPermit<'_>> {
        tokio::select! {
            first = self.ctrl.quiesce_guard.acquire() => {
                let ctrl_permit = first.ok()?;
                // While this permit is outstanding (in addition to any others
                // issued in the future), this task cannot transition into the
                // quiesced state since it is granting access to instance
                // sources through the issued DispCtx(s).
                //
                // Only once all of the `AsyncCtxPermit`s have been expired can
                // we signal through `quiesce_guard` that this task could be
                // transitioned into the quiesced state.
                ctrl_permit.forget();
                let old = self.ctx_count.swap(1, Ordering::SeqCst);
                assert_eq!(old, 0);
                self.ctx_live.add_permits(1);
                Some(AsyncCtxPermit { parent: self })
            },
            existing = self.ctx_live.acquire() => {
                // One or more `AsyncCtxPermit` instances are outstanding.
                let live_permit = existing.ok()?;
                let old = self.ctx_count.fetch_add(1, Ordering::SeqCst);
                assert!(old > 0);
                drop(live_permit);
                Some(AsyncCtxPermit { parent: self })
            }
        }
    }

    fn expire_permit(&self) {
        let was_last = self.ctx_count.fetch_sub(1, Ordering::SeqCst) == 1;
        if was_last {
            // The only other place where `ctx_live` is acquired is in
            // `acquire_permit` where there are no `await` statements after
            // acquisition, ensuring that it will finish its manipulation of the
            // `ctx_count` atomic prior to returning control.  With `AsyncCtx`
            // only ever being passed to async tasks by reference, this should
            // prevent it from being manipulated from multiple threads
            // simultaneously, so the acquisition here should be guaranteed.
            let live_permit = self.ctx_live.try_acquire().unwrap();
            live_permit.forget();

            // With the last of the `AsyncCtxPermit`s expired, this task can be
            // counted as available to enter the quiesced state once again.
            self.ctrl.quiesce_guard.add_permits(1);
        }
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
    pub fn spawn_async<T, F>(&self, task: T) -> AsyncTaskId
    where
        T: FnOnce(AsyncCtx) -> F,
        F: Future<Output = ()> + Send + 'static,
    {
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

pub(crate) struct AsyncCtxPermit<'a> {
    parent: &'a AsyncCtx,
}
impl Drop for AsyncCtxPermit<'_> {
    fn drop(&mut self) {
        self.parent.expire_permit();
    }
}
