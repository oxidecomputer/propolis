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

    /// Book-keeping for tracking outstanding requests from the block device
    ///
    /// The `Request::new_*` methods, called by the block device, explicitly initialize
    /// this to `None`. Upon getting the request from the block device and before we
    /// submit it to the backend, we update this to point to the correct shared reference.
    /// See [`Request::track_outstanding`].
    outstanding: Option<Arc<OutstandingReqs>>,
}
impl Request {
    pub fn new_read(
        off: usize,
        regions: Vec<GuestRegion>,
        donef: Box<CompleteFn>,
    ) -> Self {
        let op = Operation::Read(off);
        Self { op, regions, donef: Some(donef), outstanding: None }
    }

    pub fn new_write(
        off: usize,
        regions: Vec<GuestRegion>,
        donef: Box<CompleteFn>,
    ) -> Self {
        let op = Operation::Write(off);
        Self { op, regions, donef: Some(donef), outstanding: None }
    }

    pub fn new_flush(off: usize, len: usize, donef: Box<CompleteFn>) -> Self {
        let op = Operation::Flush(off, len);
        Self { op, regions: Vec::new(), donef: Some(donef), outstanding: None }
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

        // Update the outstanding I/O count
        self.outstanding
            .take()
            .expect("misssing OutstandingReqs ref")
            .decrement();
    }

    /// Update this request to plug into the outstanding I/O requests for a block device & backend.
    fn track_outstanding(&mut self, outstanding: Arc<OutstandingReqs>) {
        let old = std::mem::replace(&mut self.outstanding, Some(outstanding));
        assert!(old.is_none(), "request already tracked");
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

/// Helper type for keeping track of outstanding I/O requests received
/// from the block device and given to the block backend.
struct OutstandingReqs {
    /// Count of how many outstanding I/O requests there currently are
    count: AtomicU64,

    /// Notifier to indicate all outstanding requests have been completed
    notifier: Mutex<Option<Arc<Notify>>>,
}

impl OutstandingReqs {
    fn new() -> Self {
        OutstandingReqs { count: AtomicU64::new(0), notifier: Mutex::default() }
    }

    /// Increment the outstanding I/O count and update the `Request` so that it can
    /// decrement the count once it has been completed.
    fn increment_and_track(self: &Arc<Self>, req: &mut Request) {
        // Update outstanding count and update `Request` to track it
        self.count.fetch_add(1, Ordering::Relaxed);
        req.track_outstanding(Arc::clone(self));
    }

    /// Decrement the outstanding I/O count.
    ///
    /// If a notifier was since attached and we hit 0 outstanding requests as part
    /// of decrementing, then we also make sure to trigger the notifier indicating
    /// all currently outstanding requests have been completed.
    fn decrement(&self) {
        if self.count.fetch_sub(1, Ordering::Release) == 1 {
            std::sync::atomic::fence(Ordering::Acquire);
            if let Some(notifier) = &*self.notifier.lock().unwrap() {
                notifier.notify_one();
            }
        }
    }

    /// Create a new `Notify` object available to the next [`OutstandingReqs::decrement`] call.
    ///
    /// We create this explicitly not in `new` so that hitting 0 outstanding requests
    /// during the normal course of operation doesn't store a permit in the Notify and
    /// cause our later `notified()` future to complete too early.
    fn create_notifier(&self) {
        let mut notifier = self.notifier.lock().unwrap();
        assert!(
            notifier.is_none(),
            "outstanding requests notifier already exists"
        );

        let notify = Arc::new(Notify::new());
        *notifier = Some(Arc::clone(&notify));

        if self.count.load(Ordering::Acquire) == 0 {
            // If we hit 0 outstanding requests right before we created the notifier
            // above, then we need to make sure there's a permit available.
            notify.notify_one();
        }
    }

    /// Returns a future indicating when all outstanding requests have been completed.
    fn all_completed(&self) -> Option<BoxFuture<'static, ()>> {
        match &*self.notifier.lock().unwrap() {
            Some(notify) => {
                let notify = Arc::clone(notify);
                Some(Box::pin(async move { notify.notified().await }))
            }
            None => None,
        }
    }
}

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

    /// Book-keeping for tracking outstanding requests from the block device
    outstanding: Arc<OutstandingReqs>,
}

impl Notifier {
    pub fn new() -> Self {
        Self {
            armed: AtomicBool::new(false),
            notifier: Mutex::new(None),
            paused: AtomicBool::new(false),
            outstanding: Arc::new(OutstandingReqs::new()),
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
            self.outstanding.increment_and_track(&mut req);

            // Since a Request was successfully retrieved, no need to rearm the
            // notification trigger, just return the Request to the backend
            return Some(req);
        }

        // On the off chance that the underlying resource became available after
        // rearming the notification trigger, check again.
        self.armed.store(true, Ordering::Release);
        if let Some(mut req) = nextf() {
            // Update outstanding count and update `Request` to track it
            self.outstanding.increment_and_track(&mut req);

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

        // Hook up the notifier so that we know when we hit 0 outstanding
        // requeusts from this point on
        self.outstanding.create_notifier();
    }

    /// Returns a future indicating when all outstanding requests have been completed.
    pub fn paused(&self) -> BoxFuture<'static, ()> {
        assert!(self.paused.load(Ordering::Relaxed));

        self.outstanding
            .all_completed()
            .expect("missing outstanding requests notifier")
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
