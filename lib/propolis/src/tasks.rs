use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, Weak};
use std::task::{Context, Poll, Waker};

use thiserror::Error;

pub type NotifyFn = dyn Fn() + Send + Sync + 'static;

#[derive(Default)]
struct Control {
    waker_task: Option<Waker>,
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
    /// This notification may access synchronously access resources which would
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

    pub async fn wait_held(&mut self) {
        Held::new(self).await
    }
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
            assert!(!guard.is_held, "task already held");
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

pub struct TaskCtrl(Weak<Inner>);
impl TaskCtrl {
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
    pub fn exit(&mut self) {
        if let Some(inner) = self.0.upgrade() {
            let guard = inner.ctrl.lock().unwrap();
            inner.request_exit(guard);
        }
    }
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
