//! Implements an interface to virtualized block devices.

use std::collections::VecDeque;
use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};

use crate::common::*;
use crate::dispatch::{AsyncCtx, DispCtx, Dispatcher, SyncCtx, WakeFn};
use crate::vmm::{MemCtx, SubMapping};

use futures::future::BoxFuture;
use tokio::sync::{Notify, Semaphore};

mod file;
pub use file::FileBackend;

#[cfg(feature = "crucible")]
mod crucible;
#[cfg(feature = "crucible")]
pub use self::crucible::CrucibleBackend;

mod in_memory;
pub use in_memory::InMemoryBackend;

pub type ByteOffset = usize;
pub type ByteLen = usize;

/// Type of operations which may be issued to a virtual block device.
#[derive(Copy, Clone, Debug, PartialEq)]
pub enum Operation {
    /// Read from offset
    Read(ByteOffset),
    /// Write to offset
    Write(ByteOffset),
    /// Flush buffer(s) for [offset, offset + len)
    Flush(ByteOffset, ByteLen),
}

#[derive(Copy, Clone, Debug)]
pub enum Result {
    Success,
    Failure,
    Unsupported,
}

pub type CompleteFn =
    dyn FnOnce(Operation, Result, &DispCtx) + Send + Sync + 'static;

/// Block device operation request
pub struct Request {
    /// The type of operation requested by the block device
    op: Operation,

    /// A list of regions of guest memory to read/write into as part of the I/O request
    regions: Vec<GuestRegion>,

    /// Block device specific completion function for this I/O request
    donef: Option<Box<CompleteFn>>,

    /// A shared count of outstanding I/O requests for the block backend handling this request
    ///
    /// The `Request::new_*` methods explicitly initialize these but it is the backend
    /// Notifier that will actually replace it with the correct reference.
    /// See [`Request::attach_outstanding_notifier`].
    outstanding_reqs: Arc<AtomicU64>,

    /// A shared notifier to indicate to the block backend handling this request once
    /// all outstanding requests have been completed (i.e. `outstanding_reqs` hits 0).
    /// This is optional because we don't always want to be notified when we hit 0
    /// outstanding requests but only if the device has been paused.
    ///
    /// The `Request::new_*` methods explicitly initialize these but it is the backend
    /// Notifier that will actually replace it with the correct reference.
    /// See [`Request::attach_outstanding_notifier`].
    outstanding_notifier: Arc<Mutex<Option<Arc<Notify>>>>,
}
impl Request {
    pub fn new_read(
        off: usize,
        regions: Vec<GuestRegion>,
        donef: Box<CompleteFn>,
    ) -> Self {
        let op = Operation::Read(off);
        Self {
            op,
            regions,
            donef: Some(donef),
            outstanding_reqs: Arc::new(AtomicU64::new(0)),
            outstanding_notifier: Arc::new(Mutex::new(None)),
        }
    }

    pub fn new_write(
        off: usize,
        regions: Vec<GuestRegion>,
        donef: Box<CompleteFn>,
    ) -> Self {
        let op = Operation::Write(off);
        Self {
            op,
            regions,
            donef: Some(donef),
            outstanding_reqs: Arc::new(AtomicU64::new(0)),
            outstanding_notifier: Arc::new(Mutex::new(None)),
        }
    }

    pub fn new_flush(off: usize, len: usize, donef: Box<CompleteFn>) -> Self {
        let op = Operation::Flush(off, len);
        Self {
            op,
            regions: Vec::new(),
            donef: Some(donef),
            outstanding_reqs: Arc::new(AtomicU64::new(0)),
            outstanding_notifier: Arc::new(Mutex::new(None)),
        }
    }

    /// Type of operation being issued.
    pub fn oper(&self) -> Operation {
        self.op
    }

    /// Guest memory regions underlying the request
    pub fn regions(&self) -> &[GuestRegion] {
        &self.regions[..]
    }

    pub fn mappings<'a>(&self, mem: &'a MemCtx) -> Option<Vec<SubMapping<'a>>> {
        match &self.op {
            Operation::Read(_) => {
                self.regions.iter().map(|r| mem.writable_region(r)).collect()
            }
            Operation::Write(_) => {
                self.regions.iter().map(|r| mem.readable_region(r)).collect()
            }
            Operation::Flush(_, _) => None,
        }
    }

    /// Total length of operation
    pub fn len(&self) -> usize {
        match &self.op {
            Operation::Read(_) | Operation::Write(_) => {
                self.regions.iter().map(|r| r.1).sum()
            }
            Operation::Flush(_, len) => *len,
        }
    }

    /// Indiciate disposition of completed request
    pub fn complete(mut self, res: Result, ctx: &DispCtx) {
        let func = self.donef.take().unwrap();
        func(self.op, res, ctx);

        if self.outstanding_reqs.fetch_sub(1, Ordering::Release) == 1 {
            std::sync::atomic::fence(Ordering::Acquire);
            if let Some(notifier) = &*self.outstanding_notifier.lock().unwrap()
            {
                notifier.notify_one();
            }
        }
    }

    /// Update the `outstanding_*` references with the actual values.
    ///
    /// To avoid some `Option` shuffling, we initialize `outstanding_reqs`/`outstanding_notifier`
    /// but set them to the correct values here once the `Request` has been submitted to
    /// the backend.
    fn attach_outstanding_notifier(
        &mut self,
        outstanding_reqs: Arc<AtomicU64>,
        outstanding_notifier: Arc<Mutex<Option<Arc<Notify>>>>,
    ) {
        self.outstanding_reqs = outstanding_reqs;
        self.outstanding_notifier = outstanding_notifier;
    }
}
impl Drop for Request {
    fn drop(&mut self) {
        if self.donef.is_some() {
            panic!("request dropped prior to completion");
        }
    }
}

/// Metadata regarding a virtualized block device.
#[derive(Debug, Copy, Clone)]
pub struct DeviceInfo {
    /// Size (in bytes) per block
    pub block_size: u32,
    /// Device size in blocks (see above)
    pub total_size: u64,
    /// Is the device writable
    pub writable: bool,
}

/// API to access a virtualized block device.
pub trait Device: Send + Sync + 'static {
    /// Retreive the next request (if any)
    fn next(&self, ctx: &DispCtx) -> Option<Request>;

    fn set_notifier(&self, f: Option<Box<NotifierFn>>);
}

pub trait Backend: Send + Sync + 'static {
    fn attach(
        &self,
        dev: Arc<dyn Device>,
        disp: &Dispatcher,
    ) -> std::io::Result<()>;
    fn info(&self) -> DeviceInfo;
}

pub type NotifierFn = dyn Fn(&dyn Device, &DispCtx) + Send + Sync + 'static;

/// Notifier help by every block device used to indicate
/// to the corresponding block backends about I/O requests.
pub struct Notifier {
    /// Flag used to coalesce request notifications from the block device.
    ///
    /// If set, we've previously been notified but the backend has yet to
    /// respond to the latest notification.
    armed: AtomicBool,

    /// The backend specific notification handler
    notifier: Mutex<Option<Box<NotifierFn>>>,

    /// Whether or not we should service I/O requests
    paused: AtomicBool,

    /// Count of how many outstanding I/O requests there currently are
    outstanding_reqs: Arc<AtomicU64>,

    /// Notifier to indicate all outstanding requests have been completed
    outstanding_notifier: Arc<Mutex<Option<Arc<Notify>>>>,
}

impl Notifier {
    pub fn new() -> Self {
        Self {
            armed: AtomicBool::new(false),
            notifier: Mutex::new(None),
            paused: AtomicBool::new(false),
            outstanding_reqs: Arc::new(AtomicU64::new(0)),
            outstanding_notifier: Arc::new(Mutex::new(None)),
        }
    }
    pub fn next_arming(
        &self,
        nextf: impl Fn() -> Option<Request>,
    ) -> Option<Request> {
        if self.paused.load(Ordering::Acquire) {
            return None;
        }

        self.armed.store(false, Ordering::Release);
        if let Some(mut req) = nextf() {
            // Update outstanding count and update `Request` to track it
            self.outstanding_reqs.fetch_add(1, Ordering::Relaxed);
            req.attach_outstanding_notifier(
                Arc::clone(&self.outstanding_reqs),
                Arc::clone(&self.outstanding_notifier),
            );

            // Since a Request was successfully retrieved, no need to rearm the
            // notification trigger, just return the Request to the backend
            return Some(req);
        }

        // On the off chance that the underlying resource became available after
        // rearming the notification trigger, check again.
        self.armed.store(true, Ordering::Release);
        if let Some(mut req) = nextf() {
            // Update outstanding count and update `Request` to track it
            self.outstanding_reqs.fetch_add(1, Ordering::Relaxed);
            req.attach_outstanding_notifier(
                Arc::clone(&self.outstanding_reqs),
                Arc::clone(&self.outstanding_notifier),
            );

            self.armed.store(false, Ordering::Release);
            Some(req)
        } else {
            None
        }
    }
    pub fn notify(&self, dev: &dyn Device, ctx: &DispCtx) {
        if self.armed.load(Ordering::Acquire) {
            let inner = self.notifier.lock().unwrap();
            if let Some(func) = inner.as_ref() {
                func(dev, ctx);
            }
        }
    }
    pub fn set(&self, val: Option<Box<NotifierFn>>) {
        let mut inner = self.notifier.lock().unwrap();
        *inner = val;
    }

    /// Stop accepting requests from the block device.
    ///
    /// Given there might be in-flight requests being handled by the backend,
    /// the `Notifier::paused` method returns a future indicating
    /// when the pause operation is complete.
    pub fn pause(&self) {
        // Stop responding to any requests
        let paused = self.paused.swap(true, Ordering::Release);

        // Should not be attempting to pause while already paused
        assert!(!paused);

        // Create a new `Notify` object and stash it in the shared reference
        // given to outstanding I/O requests. We only create the `Notify`
        // object once we've received a request to stop servicing the guest.
        // We do this so that hitting 0 outstanding requests during the normal
        // course of operation doesn't store a permit in the Notify and cause
        // our later `notified()` future to complete too early.
        let mut notifier = self.outstanding_notifier.lock().unwrap();
        assert!(notifier.is_none());
        let notify = Arc::new(Notify::new());
        *notifier = Some(Arc::clone(&notify));

        if self.outstanding_reqs.load(Ordering::Acquire) == 0 {
            // If we hit 0 outstanding requests right before we created
            // the notifier above, then we need to make sure there's a permit
            // available.
            notify.notify_one();
        }
    }

    /// Returns a future indicating when all outstanding requests have been completed.
    pub fn paused(&self) -> BoxFuture<'static, ()> {
        assert!(self.paused.load(Ordering::Relaxed));

        let notifier = self.outstanding_notifier.lock().unwrap();
        let notify = Arc::clone(notifier.as_ref().unwrap());

        Box::pin(async move { notify.notified().await })
    }
}

pub type BackendProcessFn =
    dyn Fn(&Request, &DispCtx) -> std::io::Result<()> + Send + Sync + 'static;

/// Driver used to service requests from a block device with a specific backend.
pub struct Driver {
    /// The block device generating the requests to service
    bdev: Arc<dyn Device>,

    /// Backend provided handler for requests from block device
    req_handler: Box<BackendProcessFn>,

    /// Queue of I/O requests from the device ready to be serviced by the backend
    queue: Mutex<VecDeque<Request>>,

    /// Synchronization primitive used to block backend worker threads on requests in the queue
    cv: Condvar,

    /// Semaphore to block polling block device unless we have idle backend worker threads
    idle_threads: Semaphore,

    /// Notify handle used by block device to proactively inform us of any new requests
    wake: Arc<Notify>,
}

impl Driver {
    /// Create new `BackendDriver` to service requests for the given block device.
    pub fn new(
        bdev: Arc<dyn Device>,
        req_handler: Box<BackendProcessFn>,
    ) -> Self {
        let wake = Arc::new(Notify::new());
        // Wire up notifier to the block device
        let bdev_wake = Arc::clone(&wake);
        bdev.set_notifier(Some(Box::new(move |_dev, _ctx| {
            bdev_wake.notify_one()
        })));
        Self {
            bdev,
            req_handler,
            queue: Mutex::new(VecDeque::new()),
            cv: Condvar::new(),
            idle_threads: Semaphore::new(0),
            wake,
        }
    }

    /// Start the given number of worker threads and an async task to feed the worker
    /// with requests from the block device.
    pub fn spawn(
        self: &Arc<Self>,
        name: &'static str,
        worker_count: NonZeroUsize,
        disp: &Dispatcher,
    ) -> std::io::Result<()> {
        for i in 0..worker_count.get() {
            let worker_self = Arc::clone(self);

            // Configure a waker to help threads to reach their yield points
            // Doing this once (from thread 0) is adequate to wake them all.
            let wake = if i == 0 {
                let notify_self = Arc::downgrade(self);
                Some(Box::new(move |_ctx: &DispCtx| {
                    if let Some(this) = notify_self.upgrade() {
                        let _guard = this.queue.lock().unwrap();
                        this.cv.notify_all();
                    }
                }) as Box<WakeFn>)
            } else {
                None
            };

            // Spawn worker thread
            let _ = disp.spawn_sync(
                format!("{name} bdev {i}"),
                Box::new(move |mut sctx| {
                    worker_self.blocking_loop(&mut sctx);
                }),
                wake,
            )?;
        }

        // Create async task to get requests from block device and feed the worker threads
        let sched_self = Arc::clone(self);
        let sched_actx = disp.async_ctx();
        let sched_task = tokio::spawn(async move {
            let _ = sched_self.do_scheduling(&sched_actx).await;
        });
        disp.track(sched_task);

        Ok(())
    }

    /// Worker thread's main-loop: looks for requests to service in the queue.
    fn blocking_loop(&self, sctx: &mut SyncCtx) {
        let mut idled = false;
        loop {
            if sctx.check_yield() {
                break;
            }

            // Check if we've received any requests to process
            let mut guard = self.queue.lock().unwrap();
            if let Some(req) = guard.pop_front() {
                drop(guard);
                idled = false;
                let logger = sctx.log().clone();
                let ctx = sctx.dispctx();
                match (self.req_handler)(&req, &ctx) {
                    Ok(()) => req.complete(Result::Success, &ctx),
                    Err(e) => {
                        slog::error!(logger, "{e:?} error on req {:?}", req.op);
                        req.complete(Result::Failure, &ctx)
                    }
                }
            } else {
                // Wait until more requests are available
                if !idled {
                    self.idle_threads.add_permits(1);
                    idled = true;
                }
                let _guard = self
                    .cv
                    .wait_while(guard, |g| {
                        // While `sctx.check_yield()` is tempting here, it will
                        // block if this thread goes into a quiesce state,
                        // excluding all others from the queue lock.
                        g.is_empty() && !sctx.pending_reqs()
                    })
                    .unwrap();
            }
        }
    }

    /// Attempt to grab a request from the block device (if one's available)
    async fn next_req(&self, actx: &AsyncCtx) -> Option<Request> {
        loop {
            {
                let ctx = actx.dispctx().await?;
                if let Some(req) = self.bdev.next(&ctx) {
                    return Some(req);
                }
            }

            // Don't busy-loop on the device but just wait for it to wake us
            // when the next request is available
            self.wake.notified().await;
        }
    }

    /// Scheduling task body: feed worker threads with requests from block device.
    async fn do_scheduling(&self, actx: &AsyncCtx) {
        loop {
            // Are they any idle worker threads?
            let avail = self.idle_threads.acquire().await.unwrap();
            // We found an idle thread!
            // It will increase the permit count once it's done with any work
            avail.forget();

            // Get the next request to process
            if let Some(req) = self.next_req(actx).await {
                let mut queue = self.queue.lock().unwrap();
                queue.push_back(req);
                drop(queue);
                self.cv.notify_one();
            }
        }
    }
}
