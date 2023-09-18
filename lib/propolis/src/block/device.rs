// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Mechanisms required to implement a block device (frontend)

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::task::{Context, Poll, Waker};
use std::time::Instant;

use crate::block::{
    self, backend, probes, Backend, CacheMode, Device, DeviceInfo, Operation,
    ReqId, Request,
};

pub(super) struct AttachInner {
    sibling: Weak<backend::AttachInner>,
    backend: Arc<dyn Backend>,
    paused: bool,
}
impl AttachInner {
    pub(super) fn detach(dev: &Arc<Mutex<Option<Self>>>) -> Option<()> {
        let mut dev_lock = dev.lock().unwrap();
        let dev_inner = dev_lock.as_ref()?;

        let be_inner = dev_inner.sibling.upgrade()?;
        let mut be_lock = be_inner.state.lock().unwrap();
        let be_state = be_lock.as_ref()?;

        assert!(be_state.same_as_sibling(dev));
        *dev_lock = None;
        *be_lock = None;

        Some(())
    }
    fn lock_sibling(&self, f: impl FnOnce(&mut backend::AttachState)) {
        let sibling = self
            .sibling
            .upgrade()
            .expect("Device sibling should be present when attached");

        let mut guard = sibling.state.lock().unwrap();
        let sibling_state = guard.as_mut().expect(
            "Backend sibling should be present while device is attached",
        );
        f(sibling_state)
    }
    pub(super) fn new(
        be_attach: &backend::Attachment,
        backend: &Arc<dyn Backend>,
    ) -> Self {
        Self {
            sibling: Arc::downgrade(&be_attach.0),
            backend: backend.clone(),
            paused: false,
        }
    }
}

/// State held by the device about the attached (if any) backend
pub struct Attachment(pub(super) Arc<Mutex<Option<AttachInner>>>);
impl Attachment {
    pub fn new() -> Self {
        Attachment(Arc::new(Mutex::new(None)))
    }

    /// Query [`DeviceInfo`] from associated backend (if attached)
    pub fn info(&self) -> Option<DeviceInfo> {
        self.0.lock().unwrap().as_ref().map(|inner| inner.backend.info())
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
        let guard = self.0.lock().unwrap();
        if let Some(inner) = guard.as_ref() {
            if !inner.paused {
                let be = inner.backend.clone();
                drop(guard);
                be.attachment().notify();
            }
        }
    }

    /// Pause request processing for this device.
    ///
    /// Backend (if attached) will not be able to retrieving any requests from
    /// this device while paused.  The completions for any requests in flight,
    /// however, will be able to flow through.
    pub fn pause(&self) {
        let mut guard = self.0.lock().unwrap();
        if let Some(inner) = guard.as_mut() {
            inner.paused = true;
            inner.lock_sibling(|sib| {
                sib.set_paused(true);
            });
        }
    }

    /// Clear the paused state on this device, allowing the backend (if
    /// attached) to retrieve requests once again.
    pub fn resume(&self) {
        let mut guard = self.0.lock().unwrap();
        if let Some(inner) = guard.as_mut() {
            if !inner.paused {
                return;
            }

            inner.paused = false;
            inner.lock_sibling(|sib| {
                sib.set_paused(false);
            });
            let be = inner.backend.clone();
            drop(guard);
            be.attachment().notify();
        }
    }

    /// Detach from the associated (if any) backend.
    pub fn detach(&self) -> Option<()> {
        AttachInner::detach(&self.0)
    }
}

static NEXT_DEVICE_ID: AtomicU64 = AtomicU64::new(1);

/// Tracking structure for outstanding block [`Request`]s.
///
/// As requests are emitted to the associated backend, the corresponding data
/// required to indicate a completion to the guest will be stored here.  The
/// Request is tagged with a unique [`ReqId`] which is used to retrieve said
/// completion data, as well as track the time used to process the request.
///
/// Although use of [`Tracking`] is not required by the block abstraction, it is
/// here where the general USDT probes are attached.  A device which eschews its
/// use will be missing calls into those probes.
pub struct Tracking<T> {
    inner: Mutex<TrackingInner<T>>,
    wait: Arc<Mutex<TrackingWait>>,
}
struct TrackingInner<T> {
    device_id: u64,
    next_id: ReqId,
    dev: Weak<dyn Device>,
    outstanding: BTreeMap<ReqId, TrackingEntry<T>>,
}
struct TrackingEntry<T> {
    op: Operation,
    payload: T,
    /// When this request was submitted to the backend to be processed
    time_submitted: Instant,
}

impl<T> Tracking<T> {
    pub fn new(dev: Weak<dyn Device>) -> Self {
        let device_id = NEXT_DEVICE_ID.fetch_add(1, Ordering::Relaxed);
        Self {
            inner: Mutex::new(TrackingInner {
                device_id,
                next_id: ReqId::START,
                dev,
                outstanding: BTreeMap::new(),
            }),
            wait: Arc::new(Mutex::new(TrackingWait::new())),
        }
    }

    /// Record tracking in an [`Request`] prior to passing it to the associated
    /// [`Backend`].  The request will be assigned a unique [`ReqId`] which can
    /// be used to a later call to [`Tracking::complete()`] to retrieve the
    /// `payload` data required to communicate its completion.
    ///
    pub fn track(&self, mut req: Request, payload: T) -> Request {
        let now = Instant::now();
        let mut guard = self.inner.lock().unwrap();
        let began_empty = guard.outstanding.is_empty();
        let id = guard.next_id;
        guard.next_id.advance();
        let marker = TrackingMarker {
            id,
            dev: guard.dev.upgrade().expect("device still exists"),
        };
        guard.outstanding.insert(
            marker.id,
            TrackingEntry { op: req.op, payload, time_submitted: now },
        );

        let old = req.marker.replace(marker);
        assert!(old.is_none(), "request should be tracked only once");

        if began_empty {
            self.wait.lock().unwrap().clear_empty()
        }
        let devid = guard.device_id;
        match req.op {
            Operation::Read(off, len) => {
                probes::block_begin_read!(|| {
                    (devid, id, off as u64, len as u64)
                });
            }
            Operation::Write(off, len) => {
                probes::block_begin_write!(|| {
                    (devid, id, off as u64, len as u64)
                });
            }
            Operation::Flush => {
                probes::block_begin_flush!(|| { (devid, id) });
            }
        }

        req
    }

    /// Indicate the completion of a pending [`Request`], retrieving the
    /// associated payload data.  The [`block::Result`] argument is used to
    /// communicate the result through the generic block USDT probe.
    pub fn complete(&self, id: ReqId, res: block::Result) -> (Operation, T) {
        let now = Instant::now();
        let mut guard = self.inner.lock().unwrap();
        let entry = guard
            .outstanding
            .remove(&id)
            .expect("tracked request should be present");

        let devid = guard.device_id;
        let proc_ns =
            now.duration_since(entry.time_submitted).as_nanos() as u64;
        // TODO: calculate queued time
        let queue_ns = 0;
        let rescode = res as u8;
        match entry.op {
            Operation::Read(..) => {
                probes::block_complete_read!(|| {
                    (devid, id, rescode, proc_ns, queue_ns)
                });
            }
            Operation::Write(..) => {
                probes::block_complete_write!(|| {
                    (devid, id, rescode, proc_ns, queue_ns)
                });
            }
            Operation::Flush => {
                probes::block_complete_flush!(|| {
                    (devid, id, rescode, proc_ns, queue_ns)
                });
            }
        }

        if guard.outstanding.is_empty() {
            self.wait.lock().unwrap().set_empty();
        }

        (entry.op, entry.payload)
    }

    /// Query if there are any tracked requests outstanding
    pub fn any_outstanding(&self) -> bool {
        let guard = self.inner.lock().unwrap();
        !guard.outstanding.is_empty()
    }

    /// Emit a [`Future`] which will resolve when there are no tracked
    /// request outstanding in this structure.
    pub fn none_outstanding(&self) -> NoneOutstanding {
        NoneOutstanding::new(self.wait.clone())
    }
}

/// Record keeping for [`NoneOutstanding`] futures emitted by [`Tracking`]
struct TrackingWait {
    empty: bool,
    gen: usize,
    wake: Vec<Waker>,
}
impl TrackingWait {
    fn new() -> Self {
        Self { empty: true, gen: 1, wake: Vec::new() }
    }
    fn set_empty(&mut self) {
        self.empty = true;
        self.gen += 1;

        for waker in self.wake.drain(..) {
            waker.wake()
        }
    }
    fn clear_empty(&mut self) {
        self.empty = false;
    }
}

/// Future which will complete when the referenced [`Tracking`] has no more
/// requests outstanding.
pub struct NoneOutstanding {
    wait: Arc<Mutex<TrackingWait>>,
    gen: usize,
}
impl NoneOutstanding {
    fn new(wait: Arc<Mutex<TrackingWait>>) -> Self {
        Self { wait, gen: 0 }
    }
}
impl Future for NoneOutstanding {
    type Output = ();

    fn poll(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Self::Output> {
        let mut wguard = self.wait.lock().unwrap();
        if wguard.empty {
            Poll::Ready(())
        } else {
            // Add us to the waker list if the TrackingWait generation is newer
            // than this future.  That list is cleaned up by draining its
            // entries when waking them (forcing interested futures to re-add
            // themselves if need be).
            //
            // NoneOutstanding instances which are dropped before this occurs
            // will "leak" their entry in waker list insofar as it will not be
            // cleaned up until the next time the Tracking structure becomes
            // empty.  This is considered an acceptable trade-off for
            // simplicity.
            if wguard.gen > self.gen {
                wguard.wake.push(cx.waker().clone());
                let gen = wguard.gen;
                drop(wguard);
                self.gen = gen;
            }
            Poll::Pending
        }
    }
}

pub(super) struct TrackingMarker {
    id: ReqId,
    dev: Arc<dyn Device>,
}
impl TrackingMarker {
    pub(super) fn complete(self, res: block::Result) {
        self.dev.complete(res, self.id);
    }
}
