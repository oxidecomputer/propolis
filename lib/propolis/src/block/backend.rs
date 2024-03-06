// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Mechanisms required to implement a block backend

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Condvar, Mutex, Weak};
use std::task::{Context, Poll};

use crate::accessors::MemAccessor;
use crate::block::{device, Device, Request};

use pin_project_lite::pin_project;
use tokio::sync::{futures::Notified, Notify};

/// Reason why next request is unavailable from associated device
pub enum ReqError {
    /// No request is pending from the device
    NonePending,
    /// Processing of requests from the device is paused
    Paused,
    /// Backend is not attached to any device
    Detached,
    /// Backend is halting workers
    Halted,
}

pub(super) struct AttachState {
    sibling: Weak<Mutex<Option<device::AttachInner>>>,
    device: Arc<dyn Device>,
    acc_mem: MemAccessor,
    dev_is_paused: bool,
    backend_is_halted: bool,
}
impl AttachState {
    fn next_req(&self) -> Result<Request, ReqError> {
        if self.backend_is_halted {
            // The backend being halted is the most pressing status to consider,
            // so it must be checked first
            Err(ReqError::Halted)
        } else if self.dev_is_paused {
            // Do not allow the backend to pull any requests while the device is
            // in the paused state
            Err(ReqError::Paused)
        } else {
            self.device.next().ok_or(ReqError::NonePending)
        }
    }
    pub(super) fn new(
        dev_attach: &device::Attachment,
        device: &Arc<dyn Device>,
    ) -> Self {
        Self {
            sibling: Arc::downgrade(&dev_attach.0),
            device: device.clone(),
            acc_mem: device.accessor_mem(),
            dev_is_paused: false,
            backend_is_halted: false,
        }
    }
    pub(super) fn set_paused(&mut self, is_paused: bool) {
        self.dev_is_paused = is_paused;
    }
    pub(super) fn same_as_sibling(
        &self,
        other: &Arc<Mutex<Option<device::AttachInner>>>,
    ) -> bool {
        self.sibling.ptr_eq(&Arc::downgrade(other))
    }
}
pub(super) struct AttachInner {
    pub(super) state: Mutex<Option<AttachState>>,
    req_notifier: Notify,
    cv: Condvar,
}
impl AttachInner {
    fn new() -> Self {
        Self {
            state: Mutex::new(None),
            req_notifier: Notify::new(),
            cv: Condvar::new(),
        }
    }
}

/// State held by the backend about the attached (if any) device
pub struct Attachment(pub(super) Arc<AttachInner>);
impl Attachment {
    pub fn new() -> Self {
        Attachment(Arc::new(AttachInner::new()))
    }

    /// Attempt to retrieve the next [`Request`] from the attached (if any)
    /// device.
    ///
    /// Will return an error if:
    ///
    /// - No device is attached
    /// - The device is paused
    /// - The backend is halted
    /// - No requests are queued in the device
    pub fn next_req(&self) -> Result<Request, ReqError> {
        let guard = self.0.state.lock().unwrap();
        let inner = guard.as_ref().ok_or(ReqError::Detached)?;
        inner.next_req()
    }

    /// Block (synchronously) in order to retrieve the next [`Request`] from the
    /// device.  Will return [`None`] if no device is attached, or the backend
    /// is halted, otherwise it will block until a request is available.
    pub fn block_for_req(&self) -> Option<Request> {
        let mut guard = self.0.state.lock().unwrap();
        loop {
            // bail if not attached
            let inner = guard.as_ref()?;
            if inner.backend_is_halted {
                return None;
            }

            if let Ok(req) = inner.next_req() {
                return Some(req);
            }

            guard = self.0.cv.wait(guard).unwrap();
        }
    }

    /// Wait (via a [`Future`]) to retrieve the next [`Request`] from the
    /// device.  Will return [`None`] if no device is attached, or the backend
    /// is halted.
    pub fn wait_for_req(&self) -> WaitForReq {
        WaitForReq { attachment: self, wait: self.0.req_notifier.notified() }
    }

    /// Run provided function against [`MemAccessor`] for this backend.
    ///
    /// Intended to provide caller with means of creating/associated child
    /// accessors.
    pub fn accessor_mem<R>(
        &self,
        f: impl FnOnce(Option<&MemAccessor>) -> R,
    ) -> R {
        match self.0.state.lock().unwrap().as_ref() {
            Some(inner) => f(Some(&inner.acc_mem)),
            None => f(None),
        }
    }

    /// Assert halted state on Attachment
    pub fn halt(&self) {
        if let Some(state) = self.0.state.lock().unwrap().as_mut() {
            state.backend_is_halted = true;
        }
        self.notify();
    }

    /// Clear halted state from Attachment
    pub fn start(&self) {
        if let Some(state) = self.0.state.lock().unwrap().as_mut() {
            state.backend_is_halted = false;
        }
        self.notify();
    }

    /// Detach from the associated (if any) device.
    pub fn detach(&self) -> Option<()> {
        // lock ordering demands we approach this from the device side
        let be_lock = self.0.state.lock().unwrap();
        let be_inner = be_lock.as_ref()?;
        let dev_inner = be_inner.sibling.upgrade()?;
        drop(be_lock);

        device::AttachInner::detach(&dev_inner)
    }

    /// Notify any [blocked](Self::block_for_req()) or
    /// [waiting](Self::wait_for_req()) tasks of a state change.  This could be
    /// a change to the device, to the backend, or simply new request(s)
    /// available.
    pub fn notify(&self) {
        // TODO: avoid thundering herd?
        self.0.req_notifier.notify_waiters();
        let _guard = self.0.state.lock().unwrap();
        self.0.cv.notify_all();
    }
}

pin_project! {
    /// Future returned from [`Attachment::wait_for_req()`]
    ///
    /// This future is not fused, so it can be repeatedly polled for additional
    /// [`Request`]s as they become available.
    pub struct WaitForReq<'a> {
        attachment: &'a Attachment,
        #[pin]
        wait: Notified<'a>,
    }
}
impl Future for WaitForReq<'_> {
    type Output = Option<Request>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut this = self.project();
        loop {
            match this.attachment.next_req() {
                Ok(req) => return Poll::Ready(Some(req)),
                Err(ReqError::Detached) | Err(ReqError::Halted) => {
                    // Let the consumer know that they should bail
                    return Poll::Ready(None);
                }
                Err(ReqError::NonePending) | Err(ReqError::Paused) => {
                    if let Poll::Ready(_) =
                        Notified::poll(this.wait.as_mut(), cx)
                    {
                        // The `Notified` future is fused, so we must "refresh"
                        // prior to any subsequent attempts to poll it after it
                        // emits `Ready`
                        this.wait
                            .set(this.attachment.0.req_notifier.notified());

                        // Take another lap if woken by the notifier to check
                        // for a pending request
                        continue;
                    }
                    return Poll::Pending;
                }
            }
        }
    }
}
