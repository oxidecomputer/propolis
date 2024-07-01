// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::any::Any;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, Weak};
use std::task::{Context, Poll, Waker};
use std::thread;

use futures::stream::{FuturesUnordered, StreamExt};
use thiserror::Error;
use tokio::task;

pub type NotifyFn = dyn Fn() + Send + Sync + 'static;

#[derive(Default)]
struct Control {
    /// Waker for a future polling on events via [TaskHdl]
    waker_task: Option<Waker>,
    /// Waker for a future polling on events via [TaskCtrl]
    waker_ctrl: Option<Waker>,
    should_exit: bool,
    should_hold: bool,
    is_held: bool,
}
impl Control {
    fn wake_task(&self) {
        if let Some(w) = self.waker_task.as_ref() {
            w.wake_by_ref();
        }
    }
    fn wake_ctrl(&self) {
        if let Some(w) = self.waker_ctrl.as_ref() {
            w.wake_by_ref();
        }
    }
}
struct Inner {
    /// The `Control` of a task can be manipulated from both sync and async
    /// contexts.  Care should be taken not to otherwise block while holding the
    /// mutex guard, as it could cause an undue stall for any async pollers.
    ctrl: Mutex<Control>,
    cv: Condvar,
    notify_fn: Option<Box<NotifyFn>>,
}
impl Inner {
    fn request_hold<'a>(
        &'a self,
        mut guard: MutexGuard<'a, Control>,
        ctrl_waker: Option<Waker>,
    ) -> MutexGuard<'a, Control> {
        guard.should_hold = true;
        if let Some(waker) = ctrl_waker {
            guard.waker_ctrl = Some(waker);
        }
        guard.wake_task();
        self.cv.notify_all();
        self.notify_task(guard)
    }
    fn unrequest_hold(&self, guard: &mut MutexGuard<Control>) {
        guard.should_hold = false;
        guard.waker_ctrl = None;
        // If the task was not held already, and has not been requested to exit,
        // then removing the hold request counts as a state of no pending events
        let has_event = guard.is_held || guard.should_exit;
        if has_event {
            guard.wake_task();
            self.cv.notify_all();
        }
    }
    fn request_exit(&self, mut guard: MutexGuard<Control>) {
        guard.should_exit = true;
        guard.wake_task();
        self.cv.notify_all();
        let _guard = self.notify_task(guard);
    }

    /// Call notifier function (if one exists) for task.
    ///
    /// This notification may synchronously access resources which would
    /// otherwise be exclusive of the `MutexGuard` held on the [`Control`], so
    /// the said guard _must_ be dropped while calling the notifier.
    fn notify_task<'a>(
        &'a self,
        guard: MutexGuard<'a, Control>,
    ) -> MutexGuard<'a, Control> {
        match self.notify_fn.as_ref() {
            // Task notification only required if it is not already held
            Some(notify) if !guard.is_held => {
                drop(guard);
                notify();
                self.ctrl.lock().unwrap()
            }
            _ => guard,
        }
    }

    fn set_held(
        &self,
        guard: &mut MutexGuard<Control>,
        task_waker: Option<Waker>,
    ) {
        guard.is_held = true;
        if let Some(waker) = task_waker {
            guard.waker_task = Some(waker);
        }
        // Notify ctrl of changed state
        guard.wake_ctrl();
        self.cv.notify_all();
    }
    fn set_unheld(&self, guard: &mut MutexGuard<Control>) {
        guard.is_held = false;
        guard.waker_task = None;
        // Notify ctrl of changed state
        guard.wake_ctrl();
        self.cv.notify_all();
    }
}

#[derive(Copy, Clone, Debug)]
pub enum Event {
    Hold,
    Exit,
}

/// Handle held by a task, used to heed requests (via its corresponding
/// [TaskCtrl]) to hold or exit execution.
pub struct TaskHdl(Arc<Inner>);
impl TaskHdl {
    pub fn new(notify_fn: Option<Box<NotifyFn>>) -> (Self, TaskCtrl) {
        Self::new_inner(false, notify_fn)
    }
    pub fn new_held(notify_fn: Option<Box<NotifyFn>>) -> (Self, TaskCtrl) {
        Self::new_inner(true, notify_fn)
    }
    fn new_inner(
        initial_hold: bool,
        notify_fn: Option<Box<NotifyFn>>,
    ) -> (Self, TaskCtrl) {
        let inner = Arc::new(Inner {
            ctrl: Mutex::new(Control {
                should_hold: initial_hold,
                ..Default::default()
            }),
            cv: Condvar::new(),
            notify_fn,
        });
        let ctrl = TaskCtrl(Arc::downgrade(&inner));
        (Self(inner), ctrl)
    }

    /// Check for a pending control event for this task.
    pub fn pending_event(&self) -> Option<Event> {
        let ctrl = self.0.ctrl.lock().unwrap();
        if ctrl.should_exit {
            Some(Event::Exit)
        } else if ctrl.should_hold {
            Some(Event::Hold)
        } else {
            None
        }
    }

    /// Enter this task into a `held` state.  It will return when the task has
    /// been requested to exit or when no request for a hold remains.
    pub fn hold(&self) {
        let mut guard = self.0.ctrl.lock().unwrap();
        if guard.should_exit || !guard.should_hold {
            return;
        }
        assert!(!guard.is_held, "task already held");
        guard.is_held = true;
        let cv = &self.0.cv;
        cv.notify_all();
        let mut guard =
            cv.wait_while(guard, |g| g.should_hold && !g.should_exit).unwrap();
        guard.is_held = false;
    }

    /// Immediately force the task into a `held` state, as if the [TaskCtrl] had
    /// requested such a hold.  Like [TaskHdl::hold], it will return when an
    /// exit is requested, or the requested hold is cleared.
    pub fn force_hold(&self) {
        let mut guard = self.0.ctrl.lock().unwrap();
        if guard.should_exit {
            return;
        }
        // induce ourself into the held state
        guard.should_hold = true;
        assert!(!guard.is_held, "task already held");
        guard.is_held = true;
        let cv = &self.0.cv;
        cv.notify_all();
        let mut guard =
            cv.wait_while(guard, |g| g.should_hold && !g.should_exit).unwrap();
        guard.is_held = false;
    }

    /// Async interface equivalent to [TaskHdl::hold]
    ///
    /// Places the task in the `held` state while the future is pending.
    ///
    /// The future will become [Ready](Poll::Ready) when the task is requested
    /// to exit, or the hold request is cleared.
    pub async fn wait_held(&mut self) {
        Held::new(self).await
    }

    /// Async interface equivalent to [TaskHdl::pending_event]
    ///
    /// Emits a result when a `hold` or `exit` request is made on this task.
    pub async fn get_event(&mut self) -> Event {
        GetEvent::new(self).await
    }
}

struct Held<'a> {
    hdl: &'a mut TaskHdl,
    held: bool,
}
impl<'a> Held<'a> {
    fn new(hdl: &'a mut TaskHdl) -> Self {
        Self { hdl, held: false }
    }
}
impl Future for Held<'_> {
    type Output = ();

    fn poll(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Self::Output> {
        let mut guard = self.hdl.0.ctrl.lock().unwrap();
        if guard.should_exit || !guard.should_hold {
            if self.held {
                self.hdl.0.set_unheld(&mut guard);
                drop(guard);
                self.held = false;
            }
            Poll::Ready(())
        } else {
            self.hdl.0.set_held(&mut guard, Some(cx.waker().clone()));
            drop(guard);
            self.held = true;
            Poll::Pending
        }
    }
}
impl Drop for Held<'_> {
    fn drop(&mut self) {
        if self.held {
            let mut guard = self.hdl.0.ctrl.lock().unwrap();
            self.hdl.0.set_unheld(&mut guard);
        }
    }
}

struct GetEvent<'a> {
    hdl: &'a mut TaskHdl,
    waiting: bool,
}
impl<'a> GetEvent<'a> {
    fn new(hdl: &'a mut TaskHdl) -> Self {
        Self { hdl, waiting: false }
    }
}
impl Future for GetEvent<'_> {
    type Output = Event;

    fn poll(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Self::Output> {
        let mut guard = self.hdl.0.ctrl.lock().unwrap();
        if guard.should_exit {
            Poll::Ready(Event::Exit)
        } else if guard.should_hold {
            Poll::Ready(Event::Hold)
        } else {
            guard.waker_task = Some(cx.waker().clone());
            drop(guard);
            self.waiting = true;
            Poll::Pending
        }
    }
}
impl Drop for GetEvent<'_> {
    fn drop(&mut self) {
        if self.waiting {
            let mut guard = self.hdl.0.ctrl.lock().unwrap();
            guard.waker_task = None;
        }
    }
}
#[derive(Debug, Eq, PartialEq, Error)]
pub enum Error {
    #[error("task already exited")]
    Exited,
    #[error("task is marked to exit")]
    ExitInProgress,
    #[error("task marked to run while waiting for hold")]
    HoldInterrupted,
}

/// Exercise control of a task via requests through its [TaskHdl]
pub struct TaskCtrl(Weak<Inner>);
impl TaskCtrl {
    /// Clear any requested hold on the task.  It will fail if the task has been
    /// requested to exit (via [TaskCtrl::exit]) or has exited on its own.
    pub fn run(&mut self) -> Result<(), Error> {
        let inner = self.0.upgrade().ok_or_else(|| Error::Exited)?;
        let mut guard = inner.ctrl.lock().unwrap();
        if guard.should_exit {
            Err(Error::ExitInProgress)
        } else {
            inner.unrequest_hold(&mut guard);
            Ok(())
        }
    }

    /// Request that the task enter the `held` state.  This will return when the
    /// task enters such a state, or (for whatever reason) has exited.
    pub fn hold(&mut self) -> Result<(), Error> {
        let inner = self.0.upgrade().ok_or_else(|| Error::Exited)?;
        let mut guard = inner.ctrl.lock().unwrap();
        if guard.should_exit {
            Err(Error::ExitInProgress)
        } else {
            guard = inner.request_hold(guard, None);
            guard = inner
                .cv
                .wait_while(guard, |g| {
                    (!g.is_held && g.should_hold) && !g.should_exit
                })
                .unwrap();
            if guard.should_exit {
                Err(Error::ExitInProgress)
            } else if !guard.should_hold {
                // Someone else swooped into to clear the hold
                Err(Error::HoldInterrupted)
            } else {
                Ok(())
            }
        }
    }
    /// Request that the task exit.  Returns immediately, without waiting for
    /// said exit to actually occur.
    pub fn exit(&mut self) {
        if let Some(inner) = self.0.upgrade() {
            let guard = inner.ctrl.lock().unwrap();
            inner.request_exit(guard);
        }
    }

    /// Async interface equivalent to [TaskCtrl::hold]
    ///
    /// Requests that the task enter the `held` state.  The future becomes
    /// [ready](Poll::Ready) when the task enters `held` state or exits.
    pub async fn held(&mut self) -> Result<(), Error> {
        CtrlHeld::new(self).await
    }
}

struct CtrlHeld<'a> {
    tc: &'a mut TaskCtrl,
    pending_request: bool,
}
impl<'a> CtrlHeld<'a> {
    fn new(tc: &'a mut TaskCtrl) -> Self {
        Self { tc, pending_request: false }
    }
}
impl Future for CtrlHeld<'_> {
    type Output = Result<(), Error>;

    fn poll(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Self::Output> {
        if let Some(inner) = self.tc.0.upgrade() {
            let mut ctrl = inner.ctrl.lock().unwrap();
            if ctrl.should_exit {
                ctrl.waker_ctrl = None;
                self.pending_request = false;
                return Poll::Ready(Err(Error::ExitInProgress));
            } else if ctrl.is_held {
                ctrl.waker_ctrl = None;
                self.pending_request = false;
                return Poll::Ready(Ok(()));
            }

            if !ctrl.should_hold {
                if self.pending_request {
                    // Someone else swooped into to clear the hold
                    return Poll::Ready(Err(Error::HoldInterrupted));
                }
                let _ctrl = inner.request_hold(ctrl, Some(cx.waker().clone()));
                self.pending_request = true;
            }
            Poll::Pending
        } else {
            Poll::Ready(Err(Error::Exited))
        }
    }
}
impl Drop for CtrlHeld<'_> {
    fn drop(&mut self) {
        // Clean up the hold request if being cancelled
        if self.pending_request {
            if let Some(inner) = self.tc.0.upgrade() {
                let mut ctrl = inner.ctrl.lock().unwrap();
                inner.unrequest_hold(&mut ctrl);
            }
        }
    }
}

/// Holds a group of tokio task [task::JoinHandle]s to be later joined as a
/// group when they have all concluded.
pub struct TaskGroup(tokio::sync::Mutex<Vec<task::JoinHandle<()>>>);
impl TaskGroup {
    pub fn new() -> Self {
        Self(tokio::sync::Mutex::new(Vec::new()))
    }

    /// Add to the group of contained tasks
    pub async fn extend<I>(&self, tasks: I)
    where
        I: Iterator<Item = task::JoinHandle<()>>,
    {
        let mut guard = self.0.lock().await;
        guard.extend(tasks);
    }

    /// Block until all held tasks have been joined, returning any resulting
    /// [task::JoinError]s after doing so.
    pub async fn block_until_joined(&self) -> Option<Vec<task::JoinError>> {
        let mut guard = self.0.lock().await;
        let workers = std::mem::replace(&mut *guard, Vec::new());
        if workers.is_empty() {
            return None;
        }

        let errors = FuturesUnordered::from_iter(workers)
            .filter_map(|res| futures::future::ready(res.err()))
            .collect::<Vec<_>>()
            .await;

        if errors.is_empty() {
            None
        } else {
            Some(errors)
        }
    }
}

/// Convenience type alias for the [thread::JoinHandle::join()] error type
pub type ThreadErr = Box<dyn Any + Send + 'static>;

/// Holds a group of thread [thread::JoinHandle]s to be later joined as a group
/// when they have all concluded.
pub struct ThreadGroup(Mutex<Vec<thread::JoinHandle<()>>>);
impl ThreadGroup {
    pub fn new() -> Self {
        Self(Mutex::new(Vec::new()))
    }

    /// Add to group of contained threads
    ///
    /// The first (if any) [std::io::Error] encountered while among the
    /// `threads` items will determine the return from this function.  All
    /// non-error [thread::JoinHandle]s will be added to the group even if one
    /// or more items are an `Error`.
    pub fn extend<I>(&self, threads: I) -> std::io::Result<()>
    where
        I: Iterator<Item = std::io::Result<thread::JoinHandle<()>>>,
    {
        let mut guard = self.0.lock().unwrap();
        let mut status = Ok(());
        for result in threads {
            match result {
                Err(e) => {
                    // record the first error
                    if status.is_ok() {
                        status = Err(e);
                    }
                }
                Ok(hdl) => guard.push(hdl),
            }
        }
        status
    }

    /// Block until all contained [thread::JoinHandle]s have been joined,
    /// returning any resulting [ThreadErr]s after doing so.
    pub fn block_until_joined(&self) -> Option<Vec<ThreadErr>> {
        let mut guard = self.0.lock().unwrap();
        let errors =
            guard.drain(..).filter_map(|t| t.join().err()).collect::<Vec<_>>();
        if errors.is_empty() {
            None
        } else {
            Some(errors)
        }
    }
}
