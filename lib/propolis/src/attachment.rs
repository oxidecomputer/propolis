// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// Frontend (emulated device) attachment point
pub struct FrontAttachment<T>(Mutex<AttachPoint<T>>);
impl<T> FrontAttachment<T> {
    pub fn new() -> Self {
        Self(Mutex::new(AttachPoint::default()))
    }
    pub fn access<R>(
        &self,
        when_attached: impl FnOnce(&T, &AttachState) -> R,
    ) -> Option<R> {
        self.0.lock().unwrap().access_fallback(when_attached, |_state| ())
    }
    pub fn access_fallback<R>(
        &self,
        when_attached: impl FnOnce(&T, &AttachState) -> R,
        fallback: impl FnOnce(&AttachState) -> (),
    ) -> Option<R> {
        self.0.lock().unwrap().access_fallback(when_attached, fallback)
    }
}

/// Backend (resource provider) attachment point
pub struct BackAttachment<T>(Mutex<AttachPoint<T>>);
impl<T> BackAttachment<T> {
    pub fn new() -> Self {
        Self(Mutex::new(AttachPoint::default()))
    }
    pub fn access<R>(
        &self,
        when_attached: impl FnOnce(&T, &AttachState) -> R,
    ) -> Option<R> {
        self.0.lock().unwrap().access_fallback(when_attached, |_state| ())
    }
    pub fn access_fallback<R>(
        &self,
        when_attached: impl FnOnce(&T, &AttachState) -> R,
        fallback: impl FnOnce(&AttachState) -> (),
    ) -> Option<R> {
        self.0.lock().unwrap().access_fallback(when_attached, fallback)
    }
}

enum AttachPoint<T> {
    Attached(Arc<(AttachState, T)>),
    Detached(AttachState),
}
impl<T> Default for AttachPoint<T> {
    fn default() -> Self {
        Self::Detached(AttachState::new())
    }
}
impl<T> AttachPoint<T> {
    fn access_fallback<R>(
        &self,
        when_attached: impl FnOnce(&T, &AttachState) -> R,
        fallback: impl FnOnce(&AttachState) -> (),
    ) -> Option<R> {
        match self {
            AttachPoint::Attached(data) => {
                Some(when_attached(&data.1, &data.0))
            }
            AttachPoint::Detached(state) => {
                fallback(state);
                None
            }
        }
    }
}

/// State of an Attachment ([FrontAttachment] or [BackAttachment])
#[derive(Clone)]
pub struct AttachState(Arc<AttachStateInner>);
impl AttachState {
    /// Is this Attachment attached to a corresponding peer?
    ///
    /// Because the [AttachState] can be cloned to outlive its initial access
    /// via [FrontAttachment::access] or [BackAttachment::access], the ability
    /// to check attachedness remains relevant.
    pub fn is_attached(&self) -> bool {
        self.0.is_attached.load(Ordering::Acquire)
    }
    /// Is the Attachment frontend paused?
    pub fn is_paused(&self) -> bool {
        self.0.is_paused.load(Ordering::Acquire)
    }
    /// Is the Attachmend backend stopped (from processing additional requests)
    pub fn is_stopped(&self) -> bool {
        self.0.is_stopped.load(Ordering::Acquire)
    }
    /// Notify waiters of event (either attachment state change, or of
    /// attachment-specific events, such as new pending requests)
    pub fn notify(&self) {
        self.0.notify.notify_waiters()
    }
    /// Wait for notification relating to Attachment
    pub fn notified(&self) -> tokio::sync::futures::Notified<'_> {
        self.0.notify.notified()
    }
    /// Mark Attachment frontend as paused
    pub fn pause(&self) {
        if !self.0.is_paused.swap(true, Ordering::Release) {
            self.0.notify.notify_waiters();
        }
    }
    /// Mark Attachment frontend as resumed (no longer paused)
    pub fn resume(&self) {
        if self.0.is_paused.swap(false, Ordering::Release) {
            self.0.notify.notify_waiters();
        }
    }
    /// Mark Attachment backend as stopped
    pub fn stop(&self) {
        if !self.0.is_stopped.swap(true, Ordering::Release) {
            self.0.notify.notify_waiters();
        }
    }
    /// Mark Attachment backend as started (no longer stopped)
    pub fn start(&self) {
        if self.0.is_stopped.swap(false, Ordering::Release) {
            self.0.notify.notify_waiters();
        }
    }

    fn new() -> Self {
        Self(Arc::new(AttachStateInner::default()))
    }
    fn merge(front: &Self, back: &Self) -> Self {
        // Because `is_paused` state is a property of the frontend (suspending
        // emission of requests to be processed by backend) and `is_stopped`
        // state is a property of the backend (stopping workers, so none are
        // attempting to process requests from frontend), we use those bits of
        // state from their respective "owners".
        Self(Arc::new(AttachStateInner {
            is_attached: AtomicBool::new(false),
            is_paused: AtomicBool::new(front.is_paused()),
            is_stopped: AtomicBool::new(back.is_stopped()),
            ..Default::default()
        }))
    }
    fn split(&self) -> (Self, Self) {
        (
            Self(Arc::new(AttachStateInner {
                is_paused: AtomicBool::new(self.is_paused()),
                ..Default::default()
            })),
            Self(Arc::new(AttachStateInner {
                is_stopped: AtomicBool::new(self.is_stopped()),
                ..Default::default()
            })),
        )
    }

    fn attach(&self) {
        let prev = self.0.is_attached.swap(true, Ordering::Release);
        self.0.notify.notify_waiters();
        assert!(!prev, "AttachState was not 'detached' during attach()");
    }
    fn detach(&self) {
        let prev = self.0.is_attached.swap(false, Ordering::Release);
        self.0.notify.notify_waiters();
        assert!(prev, "AttachState was not 'attached' during detach()");
    }
}
struct AttachStateInner {
    is_attached: AtomicBool,
    is_paused: AtomicBool,
    is_stopped: AtomicBool,
    notify: tokio::sync::Notify,
}
impl Default for AttachStateInner {
    fn default() -> Self {
        Self {
            is_attached: AtomicBool::new(false),
            is_paused: AtomicBool::new(false),
            is_stopped: AtomicBool::new(true),
            notify: tokio::sync::Notify::new(),
        }
    }
}

/// Errors possible during attempted [attach] call
#[derive(thiserror::Error, Debug)]
pub enum AttachError {
    #[error("frontend is already attached")]
    FrontAttached,
    #[error("backend is already attached")]
    BackAttached,
}

/// Attempt to attach `front_attach` and `back_attach`.
///
/// If neither are already attached to another instance, they'll be attached
/// together and the `on_attach` handler run against a reference of the provided
/// `data` once the attachment has been made.
pub fn attach<T>(
    data: T,
    front_attach: &FrontAttachment<T>,
    back_attach: &BackAttachment<T>,
    on_attach: impl FnOnce(&T),
) -> Result<(), AttachError> {
    let mut front_lock = front_attach.0.lock().unwrap();
    let mut back_lock = back_attach.0.lock().unwrap();

    let att_state = match (&*front_lock, &*back_lock) {
        (
            AttachPoint::Detached(front_state),
            AttachPoint::Detached(back_state),
        ) => Ok(AttachState::merge(front_state, back_state)),
        (AttachPoint::Attached(..), _) => Err(AttachError::FrontAttached),
        (_, AttachPoint::Attached(..)) => Err(AttachError::BackAttached),
    }?;

    let att_data = Arc::new((att_state, data));
    *front_lock = AttachPoint::Attached(att_data.clone());
    *back_lock = AttachPoint::Attached(att_data.clone());

    att_data.0.attach();
    on_attach(&att_data.1);

    Ok(())
}

/// Errors possible during attempted [detach] call
#[derive(thiserror::Error, Debug)]
pub enum DetachError {
    #[error("frontend is not attached")]
    FrontNotAttached,
    #[error("backend is not attached")]
    BackNotAttached,
    #[error("frontend and backend are not attached to each other")]
    AttachmentMismatch,
    #[error("detach handler failed")]
    DetachFailed,
}

/// Attempt to detach `front_attach` from `back_attach`.
///
/// If those attachment points are indeed attached to each other, the
/// `on_detach` handler will be run, and if it does not yield an error, the
/// detach operation will complete successfully.
pub fn detach<T>(
    front_attach: &FrontAttachment<T>,
    back_attach: &BackAttachment<T>,
    on_detach: impl FnOnce(&T, &AttachState) -> Result<(), ()>,
) -> Result<(), DetachError> {
    let mut front_lock = front_attach.0.lock().unwrap();
    let mut back_lock = back_attach.0.lock().unwrap();

    let data = match (&*front_lock, &*back_lock) {
        (
            AttachPoint::Attached(front_data),
            AttachPoint::Attached(back_data),
        ) => {
            if !Arc::ptr_eq(front_data, back_data) {
                Err(DetachError::AttachmentMismatch)
            } else {
                Ok(front_data.clone())
            }
        }
        (AttachPoint::Detached(..), _) => Err(DetachError::FrontNotAttached),
        (_, AttachPoint::Detached(..)) => Err(DetachError::BackNotAttached),
    }?;

    on_detach(&data.1, &data.0).map_err(|_| DetachError::DetachFailed)?;
    data.0.detach();
    let (front_state, back_state) = data.0.split();

    *front_lock = AttachPoint::Detached(front_state);
    *back_lock = AttachPoint::Detached(back_state);

    Ok(())
}
