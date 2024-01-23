// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Condvar, Mutex};
use std::task::{Context, Poll};

use super::{Backend, CacheMode, Device, DeviceInfo, Request};
use crate::accessors::MemAccessor;
use crate::attachment;

use pin_project_lite::pin_project;
use tokio::sync::futures::Notified;

struct BlockData {
    device: Arc<dyn Device>,
    backend: Arc<dyn Backend>,
    acc_mem: MemAccessor,
    lock: Mutex<()>,
    cv: Condvar,
}
impl BlockData {
    fn new(device: Arc<dyn Device>, backend: Arc<dyn Backend>) -> Arc<Self> {
        let acc_mem = device.accessor_mem();
        Arc::new(Self {
            device,
            backend,
            acc_mem,
            lock: Mutex::new(()),
            cv: Condvar::new(),
        })
    }

    /// Notify any [blocked](Self::block_for_req()) tasks of a state change.
    /// This could be a change to the device, to the backend, or simply new
    /// request(s) available.
    fn notify(&self) {
        let _guard = self.lock.lock().unwrap();
        self.cv.notify_all();
    }

    fn next_req(
        &self,
        att_state: &attachment::AttachState,
    ) -> Result<Request, ReqError> {
        check_state(att_state)?;
        match self.device.next() {
            Some(req) => Ok(req),
            None => {
                check_state(att_state)?;
                Err(ReqError::NonePending)
            }
        }
    }

    fn block_for_req(
        &self,
        att_state: &attachment::AttachState,
    ) -> Option<Request> {
        loop {
            match self.next_req(att_state) {
                Ok(req) => {
                    return Some(req);
                }
                Err(err) => match err {
                    ReqError::NonePending | ReqError::Paused => {
                        let guard = self.lock.lock().unwrap();
                        // Double-check for attachment-related "error"
                        // conditions under protection of the lock before
                        // finally blocking.
                        if check_state(att_state).is_err() {
                            return None;
                        }
                        let _guard = self.cv.wait(guard).unwrap();
                        continue;
                    }
                    _ => {
                        return None;
                    }
                },
            }
        }
    }
}

fn check_state(state: &attachment::AttachState) -> Result<(), ReqError> {
    if !state.is_attached() {
        Err(ReqError::Detached)
    } else if state.is_stopped() {
        Err(ReqError::Stopped)
    } else if state.is_paused() {
        Err(ReqError::Paused)
    } else {
        Ok(())
    }
}

pub struct DeviceAttachment(attachment::FrontAttachment<Arc<BlockData>>);
impl DeviceAttachment {
    pub fn new() -> Self {
        Self(attachment::FrontAttachment::new())
    }

    /// Query [DeviceInfo] from associated backend (if attached)
    pub fn info(&self) -> Option<DeviceInfo> {
        self.0.access(|data, _state| data.backend.info())
    }

    /// Set cache mode on associated backend
    ///
    /// # Warning
    ///
    /// This is currently unimplemented!
    pub fn set_cache_mode(&self, _mode: CacheMode) {
        todo!("wire up cache mode toggling")
    }

    /// Notify attached backend of (new) pending requests
    pub fn notify(&self) {
        self.0.access(|data, state| {
            if !state.is_paused() {
                state.notify();
                data.notify();
            }
        });
    }

    /// Pause request processing for this device.
    ///
    /// Backend (if attached) will not be able to retrieving any requests from
    /// this device while paused.  The completions for any requests in flight,
    /// however, will be able to flow through.
    pub fn pause(&self) {
        self.0.access_fallback(
            |data, state| {
                state.pause();
                data.notify();
            },
            |state| state.pause(),
        );
    }

    /// Clear the paused state on this device, allowing the backend (if
    /// attached) to retrieve requests once again.
    pub fn resume(&self) {
        self.0.access_fallback(
            |data, state| {
                state.resume();
                data.notify();
            },
            |state| state.resume(),
        );
    }

    /// Attempt to detach device from backend
    pub fn detach(&self) -> Result<(), attachment::DetachError> {
        if let Some((dev, backend)) = self
            .0
            .access(|data, _state| (data.device.clone(), data.backend.clone()))
        {
            detach(dev, backend)
        } else {
            Err(attachment::DetachError::FrontNotAttached)
        }
    }
}

/// Reason why next request is unavailable from associated device
pub enum ReqError {
    /// No request is pending from the device
    NonePending,
    /// Processing of requests from the device is paused
    Paused,
    /// Backend is not attached to any device
    Detached,
    /// Backend is stopping workers
    Stopped,
}

pub struct BackendAttachment(attachment::BackAttachment<Arc<BlockData>>);
impl BackendAttachment {
    pub fn new() -> Self {
        Self(attachment::BackAttachment::new())
    }

    /// Attempt to retrieve the next [Request] from the attached (if any)
    /// device.
    ///
    /// Will return an error if:
    ///
    /// - No device is attached
    /// - The device is paused
    /// - The backend is stopped
    /// - No requests are queued in the device
    pub fn next_req(&self) -> Result<Request, ReqError> {
        match self.0.access(|data, state| data.next_req(state)) {
            None => Err(ReqError::Detached),
            Some(Err(e)) => Err(e),
            Some(Ok(r)) => Ok(r),
        }
    }

    /// Block (synchronously) in order to retrieve the next [Request] from the
    /// device.  Will return [None] if no device is attached, or the backend
    /// is stopped, otherwise it will block until a request is available.
    pub fn block_for_req(&self) -> Option<Request> {
        match self.0.access(|data, state| {
            // If we do not immediately get a request, clone the BackendData
            // so we can do our blocking through that, instead of repeated calls
            // into BackAttachment::access()
            data.next_req(state).map_err(|_e| (data.clone(), state.clone()))
        })? {
            Ok(req) => Some(req),
            Err((be, state)) => be.block_for_req(&state),
        }
    }

    /// Get a [Waiter], through which one can asynchronously poll for new
    /// requests via [`Waiter::for_req()`].  Will return [None] if no device
    /// is attached, or the backend is stopped.
    pub fn waiter(&self) -> Option<Waiter> {
        self.0.access(|data, state| Waiter(data.clone(), state.clone()))
    }

    /// Run provided function against [MemAccessor] for this backend.
    ///
    /// Intended to provide caller with means of creating/associated child
    /// accessors.
    pub fn accessor_mem<R>(
        &self,
        f: impl FnOnce(&MemAccessor) -> R,
    ) -> Option<R> {
        self.0.access(|data, _state| f(&data.acc_mem))
    }

    /// Assert stopped state on this Attachment
    pub fn stop(&self) {
        self.0.access_fallback(
            |data, state| {
                state.stop();
                data.notify();
            },
            |state| state.stop(),
        );
    }

    /// Clear stopped state from this Attachment
    pub fn start(&self) {
        self.0.access_fallback(
            |data, state| {
                state.start();
                data.notify();
            },
            |state| state.start(),
        );
    }

    /// Attempt to detach backend from device
    pub fn detach(&self) -> Result<(), attachment::DetachError> {
        if let Some((dev, backend)) = self
            .0
            .access(|data, _state| (data.device.clone(), data.backend.clone()))
        {
            detach(dev, backend)
        } else {
            Err(attachment::DetachError::BackNotAttached)
        }
    }
}

pub struct Waiter(Arc<BlockData>, attachment::AttachState);
impl Waiter {
    /// Wait (via a [Future]) to retrieve the next [Request] from the device.
    pub fn for_req(&self) -> WaitForReq<'_> {
        WaitForReq { be: &self.0, att_state: &self.1, wait: self.1.notified() }
    }
}

pin_project! {
    /// [Future] returned from [`Waiter::for_req()`]
    pub struct WaitForReq<'a> {
        be: &'a BlockData,
        att_state: &'a attachment::AttachState,
        #[pin]
        wait: Notified<'a>
    }
}
impl Future for WaitForReq<'_> {
    type Output = Option<Request>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut this = self.project();
        loop {
            match this.be.next_req(this.att_state) {
                Ok(req) => return Poll::Ready(Some(req)),
                Err(ReqError::NonePending) | Err(ReqError::Paused) => {
                    if let Poll::Ready(_) =
                        Notified::poll(this.wait.as_mut(), cx)
                    {
                        // The `Notified` future is fused, so we must "refresh"
                        // prior to any subsequent attempts to poll it after it
                        // emits `Ready`
                        this.wait.set(this.att_state.notified());

                        // Take another lap if woken by the notifier to check
                        // for a pending request
                        continue;
                    }
                    return Poll::Pending;
                }
                Err(_) => {
                    // Let the consumer know that they should bail
                    return Poll::Ready(None);
                }
            }
        }
    }
}

/// Attempt to attach a block [Device] to a block [Backend].
pub fn attach(
    device: Arc<dyn Device>,
    backend: Arc<dyn Backend>,
) -> Result<(), attachment::AttachError> {
    attachment::attach(
        BlockData::new(device.clone(), backend.clone()),
        &device.attachment().0,
        &backend.attachment().0,
        |data| {
            data.device.on_attach(data.backend.info());
        },
    )
}

fn detach(
    device: Arc<dyn Device>,
    backend: Arc<dyn Backend>,
) -> Result<(), attachment::DetachError> {
    attachment::detach(
        &device.attachment().0,
        &backend.attachment().0,
        |data, state| {
            if !state.is_stopped() {
                // Demand that backend be stopped before detaching.
                //
                // Assuming it is otherwise well behaved, such a check should
                // ensure that it is not holding on any unprocessed requests
                // from the device.
                Err(())
            } else {
                data.notify();
                Ok(())
            }
        },
    )
}
