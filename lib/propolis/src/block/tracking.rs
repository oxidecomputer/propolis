// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Mechanisms required to implement a block device

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

use crate::block::{self, probes, Device, Operation, ReqId, Request};

static NEXT_DEVICE_ID: AtomicU64 = AtomicU64::new(1);

/// A function that is called when a block operation completes.
pub trait CompletionCallback:
    Fn(Operation, block::Result, Duration) + Send + Sync + 'static
{
}

impl<T> CompletionCallback for T where
    T: Fn(Operation, block::Result, Duration) + Send + Sync + 'static
{
}

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
///
/// Each [`Tracking`] also allows one optional callback that it will call
/// whenever an I/O is completed. This can be set in the
/// [`Tracking::with_completion_callback()`] constructor, or with
/// [`Tracking::set_completion_callback()`].
pub struct Tracking<T> {
    inner: Mutex<TrackingInner<T>>,
    wait: Arc<Mutex<TrackingWait>>,
}
struct TrackingInner<T> {
    device_id: u64,
    next_id: ReqId,
    dev: Weak<dyn Device>,
    outstanding: BTreeMap<ReqId, TrackingEntry<T>>,
    on_completion: Option<Box<dyn CompletionCallback>>,
}
struct TrackingEntry<T> {
    op: Operation,
    payload: T,
    /// When this request was submitted to the backend to be processed
    time_submitted: Instant,
}

/// Track device-specific data for outstanding block [Request]s.
impl<T> Tracking<T> {
    pub fn new(dev: Weak<dyn Device>) -> Self {
        Self::new_raw(dev, None)
    }

    /// Create a new tracking object with a completion callback.
    pub fn with_completion_callback(
        dev: Weak<dyn Device>,
        cb: impl CompletionCallback,
    ) -> Self {
        Self::new_raw(dev, Some(Box::new(cb)))
    }

    fn new_raw(
        dev: Weak<dyn Device>,
        cb: Option<Box<dyn CompletionCallback>>,
    ) -> Self {
        let device_id = NEXT_DEVICE_ID.fetch_add(1, Ordering::Relaxed);
        Self {
            inner: Mutex::new(TrackingInner {
                device_id,
                next_id: ReqId::START,
                dev,
                outstanding: BTreeMap::new(),
                on_completion: cb,
            }),
            wait: Arc::new(Mutex::new(TrackingWait::new())),
        }
    }

    /// Set or overwrite the completion callback.
    ///
    /// Returns true if there was a previous callback.
    pub fn set_completion_callback(
        &self,
        cb: Box<dyn CompletionCallback>,
    ) -> bool {
        self.inner.lock().unwrap().on_completion.replace(cb).is_some()
    }

    /// Record tracking in an [`Request`] prior to passing it to the associated
    /// [`block::Backend`].  The request will be assigned a unique [`ReqId`]
    /// which can be used to a later call to [`Tracking::complete()`] to
    /// retrieve the `payload` data required to communicate its completion.
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
        let elapsed = now.duration_since(entry.time_submitted);
        let proc_ns = elapsed.as_nanos() as u64;
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

        if let Some(cb) = guard.on_completion.as_ref() {
            cb(entry.op, res, elapsed);
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
