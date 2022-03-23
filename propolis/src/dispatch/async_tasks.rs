use std::collections::hash_map::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak};

use tokio::runtime::{Builder, Handle, Runtime};
use tokio::sync::Semaphore;
use tokio::task::JoinHandle;

use super::{DispCtx, SharedCtx};

#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub struct CtxId(usize);

struct CtxCtrl {
    quiesce_guard: Semaphore,
    quiesce_notify: Semaphore,
    id: CtxId,
    container: Weak<Mutex<HashMap<CtxId, CtxEntry>>>,
}

struct CtxEntry {
    ctrl: Arc<CtxCtrl>,
    quiesced: bool,
}

struct InnerState {
    next_id: CtxId,
    quiesced: bool,
    shutdown: bool,
}

struct Inner {
    state: Mutex<InnerState>,
    ctrls: Arc<Mutex<HashMap<CtxId, CtxEntry>>>,
}
impl Inner {
    fn new() -> Self {
        Self {
            state: Mutex::new(InnerState {
                next_id: CtxId(0),
                quiesced: false,
                shutdown: false,
            }),
            ctrls: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    fn get_ctrl(&self) -> Arc<CtxCtrl> {
        let mut state = self.state.lock().unwrap();
        let mut ctrls = self.ctrls.lock().unwrap();

        let id = state.next_id;
        state.next_id.0 += 1;
        let ctrl = Arc::new(CtxCtrl {
            quiesce_guard: Semaphore::new(0),
            quiesce_notify: Semaphore::new(0),
            id,
            container: Arc::downgrade(&self.ctrls),
        });

        if !state.quiesced {
            // Make the async ctx available if tasks are not quiesced
            ctrl.quiesce_guard.add_permits(1);
        } else {
            // Notify task that the ctx is in a quiesced state
            ctrl.quiesce_notify.add_permits(1);
        }
        ctrls.insert(
            id,
            CtxEntry { ctrl: ctrl.clone(), quiesced: state.quiesced },
        );

        ctrl
    }
}

enum RuntimeBacking {
    Owned(Box<Runtime>),
    External(Handle),
}

pub(super) struct AsyncDispatch {
    inner: Inner,
    backing: Mutex<Option<RuntimeBacking>>,
    /// Tasks which should be cancelled when the dispatcher is shutdown
    tracked: Mutex<Vec<JoinHandle<()>>>,
}
impl AsyncDispatch {
    pub(super) fn new(rt_handle: Option<Handle>) -> Self {
        let backing = match rt_handle {
            Some(handle) => RuntimeBacking::External(handle),
            None => {
                let rt =
                    Builder::new_multi_thread().enable_all().build().unwrap();
                RuntimeBacking::Owned(Box::new(rt))
            }
        };
        Self {
            inner: Inner::new(),
            backing: Mutex::new(Some(backing)),
            tracked: Mutex::new(Vec::new()),
        }
    }

    /// Create a new `AsyncCtx` for an async task to access the instance state.
    pub(super) fn context(&self, shared: SharedCtx) -> AsyncCtx {
        let ctrl = self.inner.get_ctrl();
        AsyncCtx::new(
            shared.log_child(slog::o!("async_task" => ctrl.id.0)),
            ctrl,
        )
    }

    pub(super) fn quiesce_contexts(&self) {
        let mut state = self.inner.state.lock().unwrap();
        if state.quiesced {
            return;
        }
        state.quiesced = true;
        let mut ctrls = self.inner.ctrls.lock().unwrap();

        if let Some(hdl) = self.handle() {
            hdl.block_on(async {
                for (_id, task) in ctrls.iter_mut() {
                    if !task.quiesced {
                        task.quiesced = true;
                        task.ctrl.quiesce_notify.add_permits(1);
                        let permit =
                            task.ctrl.quiesce_guard.acquire().await.unwrap();
                        permit.forget();
                    }
                }
            })
        }
    }

    pub(super) fn release_contexts(&self) {
        let mut state = self.inner.state.lock().unwrap();
        if !state.quiesced {
            return;
        }
        state.quiesced = false;
        let mut ctrls = self.inner.ctrls.lock().unwrap();

        if let Some(hdl) = self.handle() {
            hdl.block_on(async {
                for (_id, task) in ctrls.iter_mut() {
                    if task.quiesced {
                        let permit =
                            task.ctrl.quiesce_notify.acquire().await.unwrap();
                        permit.forget();
                        task.ctrl.quiesce_guard.add_permits(1);
                        task.quiesced = false;
                    }
                }
            })
        }
    }

    pub(super) fn release_context(&self, id: CtxId) {
        let state = self.inner.state.lock().unwrap();
        if !state.quiesced {
            return;
        }

        let mut ctrls = self.inner.ctrls.lock().unwrap();
        if let Some(hdl) = self.handle() {
            hdl.block_on(async {
                if let Some(task) = ctrls.get_mut(&id) {
                    if task.quiesced {
                        let permit =
                            task.ctrl.quiesce_notify.acquire().await.unwrap();
                        permit.forget();
                        task.ctrl.quiesce_guard.add_permits(1);
                        task.quiesced = false;
                    }
                }
            })
        }
    }

    /// Track an async task so it is aborted when the dispatcher is shutdown
    pub(super) fn track(&self, hdl: JoinHandle<()>) {
        let mut tracked = self.tracked.lock().unwrap();
        tracked.push(hdl);
    }

    pub(super) fn shutdown(&self) {
        let mut state = self.inner.state.lock().unwrap();
        if state.shutdown {
            return;
        }
        state.shutdown = true;
        drop(state);

        let mut tracked = self.tracked.lock().unwrap();
        let to_cancel = std::mem::replace(tracked.as_mut(), Vec::new());
        for hdl in to_cancel.into_iter() {
            hdl.abort();
        }

        let mut backing = self.backing.lock().unwrap();
        match backing.take().unwrap() {
            RuntimeBacking::Owned(rt) => rt.shutdown_background(),
            RuntimeBacking::External(_) => {}
        }
    }

    pub fn handle(&self) -> Option<Handle> {
        let guard = self.backing.lock().unwrap();
        match guard.as_ref()? {
            RuntimeBacking::Owned(rt) => Some(rt.handle().clone()),
            RuntimeBacking::External(h) => Some(h.clone()),
        }
    }
}

pub struct AsyncCtx {
    shared: SharedCtx,
    ctrl: Arc<CtxCtrl>,
    ctx_live: Semaphore,
    ctx_count: AtomicUsize,
}
impl AsyncCtx {
    fn new(shared: SharedCtx, ctrl: Arc<CtxCtrl>) -> Self {
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
            inst: &self.shared.inst,
            _async_permit: Some(permit),
        })
    }

    /// Get access to `Logger` associated with this task
    pub fn log(&self) -> &slog::Logger {
        &self.shared.log
    }

    /// Return the ID for the context associated with this task.
    pub fn context_id(&self) -> CtxId {
        self.ctrl.id
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
}
impl Clone for AsyncCtx {
    /// Acquire a new `AsyncCtx`, useful for accessing instance state from
    /// emulation running in an async runtime
    fn clone(&self) -> Self {
        let inst = Weak::upgrade(&self.shared.inst).unwrap();
        inst.async_ctx()
    }
}
impl Drop for AsyncCtx {
    fn drop(&mut self) {
        assert_eq!(self.ctx_count.load(Ordering::Acquire), 0);
        if let Some(ctrls) = Weak::upgrade(&self.ctrl.container) {
            let mut guard = ctrls.lock().unwrap();
            let ent = guard.remove(&self.ctrl.id).unwrap();
            assert_eq!(Arc::as_ptr(&ent.ctrl), Arc::as_ptr(&self.ctrl));
        }
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
