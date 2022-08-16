//! Implements an interface to virtualized block devices.

use std::any::Any;
use std::collections::VecDeque;
use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, Weak};

use crate::accessors::MemAccessor;
use crate::common::*;
use crate::tasks::*;
use crate::vmm::{MemCtx, SubMapping};

use futures::future::BoxFuture;
use once_cell::sync::OnceCell;
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

pub type BlockPayload = dyn Any + Send + 'static;

/// Block device operation request
pub struct Request {
    /// The type of operation requested by the block device
    op: Operation,

    /// A list of regions of guest memory to read/write into as part of the I/O request
    regions: Vec<GuestRegion>,

    /// Block device specific completion payload for this I/O request
    payload: Option<Box<dyn Any + Send>>,

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
        payload: Box<BlockPayload>,
    ) -> Self {
        let op = Operation::Read(off);
        Self { op, regions, payload: Some(payload), outstanding: None }
    }

    pub fn new_write(
        off: usize,
        regions: Vec<GuestRegion>,
        payload: Box<BlockPayload>,
    ) -> Self {
        let op = Operation::Write(off);
        Self { op, regions, payload: Some(payload), outstanding: None }
    }

    pub fn new_flush(
        off: usize,
        len: usize,
        payload: Box<BlockPayload>,
    ) -> Self {
        let op = Operation::Flush(off, len);
        Self {
            op,
            regions: Vec::new(),
            payload: Some(payload),
            outstanding: None,
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
    pub fn complete(mut self, res: Result, dev: &dyn Device) {
        let payload = self.payload.take().unwrap();
        dev.complete(self.op, res, payload);

        // Update the outstanding I/O count
        self.outstanding
            .take()
            .expect("missing OutstandingReqs ref")
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
        if self.payload.is_some() {
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
    fn next(&self) -> Option<Request>;

    /// Complete processing of result
    fn complete(&self, op: Operation, res: Result, payload: Box<dyn Any>);

    /// Get an accessor to guest memory via the underlying device
    fn accessor_mem(&self) -> MemAccessor;

    fn set_notifier(&self, f: Option<Box<NotifierFn>>);
}

pub trait Backend: Send + Sync + 'static {
    fn attach(&self, dev: Arc<dyn Device>) -> std::io::Result<()>;
    fn info(&self) -> DeviceInfo;
    /// Process a block device request.  It is expected that the `Backend`
    /// itself will call this through some queuing driver apparatus, rather than
    /// the block device emulation itself.
    fn process(&self, req: &Request, mem: &MemCtx) -> Result;
}

pub type NotifierFn = dyn Fn(&dyn Device) + Send + Sync + 'static;

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
    pub fn notify(&self, dev: &dyn Device) {
        if self.armed.load(Ordering::Acquire) {
            let inner = self.notifier.lock().unwrap();
            if let Some(func) = inner.as_ref() {
                func(dev);
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

/// Driver used to service requests from a block device with a specific backend.
pub struct Driver {
    inner: Arc<DriverInner>,
    outer: Weak<dyn Backend>,
    name: String,
}

impl Driver {
    /// Create new `BackendDriver` to service requests for the given block device.
    pub fn new(
        outer: Weak<dyn Backend>,
        name: String,
        worker_count: NonZeroUsize,
    ) -> Self {
        Self {
            inner: Arc::new(DriverInner {
                bdev: OnceCell::new(),
                acc_mem: OnceCell::new(),
                queue: Mutex::new(VecDeque::new()),
                cv: Condvar::new(),
                idle_threads: Semaphore::new(0),
                wake: Arc::new(Notify::new()),
                task_ctrl: Mutex::new(DriverCtrls::new(worker_count)),
            }),
            outer,
            name,
        }
    }

    /// Attach driver to emulated device and spawn worker tasks for processing requests.
    pub fn attach(&self, bdev: Arc<dyn Device>) -> std::io::Result<()> {
        let be = self.outer.upgrade().expect("backend must exist");

        let _old_bdev = self.inner.bdev.set(bdev.clone());
        assert!(_old_bdev.is_ok(), "driver already attached");
        let _old_acc = self.inner.acc_mem.set(bdev.accessor_mem());
        assert!(_old_acc.is_ok(), "driver already attached");

        // Wire up notifier to the block device
        let wake = self.inner.wake.clone();
        bdev.set_notifier(Some(Box::new(move |_bdev| wake.notify_one())));

        // Spawn (held) worker tasks
        let mut tasks = self.inner.task_ctrl.lock().unwrap();
        for (i, ctrl_slot) in tasks.workers.iter_mut().enumerate() {
            let (mut task, ctrl) = TaskHdl::new_held(Some(self.worker_waker()));

            let worker_self = Arc::clone(&self.inner);
            let worker_be = be.clone();
            let _join = std::thread::Builder::new()
                .name(format!("{} {i}", &self.name))
                .spawn(move || {
                    worker_self.process_requests(&mut task, worker_be);
                })?;
            let old = ctrl_slot.replace(ctrl);
            assert!(old.is_none(), "worker {} task already exists", 1);
        }

        // Create async task to get requests from block device and feed the worker threads
        let sched_self = Arc::clone(&self.inner);
        let (mut task, ctrl) = TaskHdl::new_held(None);
        let _join = tokio::spawn(async move {
            let _ = sched_self.schedule_requests(&mut task).await;
        });
        let old = tasks.sched.replace(ctrl);
        assert!(old.is_none(), "sched task already exists");

        Ok(())
    }

    fn worker_waker(&self) -> Box<NotifyFn> {
        let this = Arc::downgrade(&self.inner);

        Box::new(move || {
            if let Some(this) = this.upgrade() {
                // Take the queue lock in order to synchronize notification with
                // any activity in the processing thread.
                let _guard = this.queue.lock().unwrap();
                this.cv.notify_all()
            }
        })
    }

    pub fn run(&self) {
        let mut ctrls = self.inner.task_ctrl.lock().unwrap();
        let _ = ctrls.sched.as_mut().unwrap().run();
        for worker in ctrls.workers.iter_mut() {
            let _ = worker.as_mut().unwrap().run();
        }
    }
    pub fn pause(&self) {
        let mut ctrls = self.inner.task_ctrl.lock().unwrap();
        let _ = ctrls.sched.as_mut().unwrap().hold();
        for worker in ctrls.workers.iter_mut() {
            let _ = worker.as_mut().unwrap().hold();
        }
    }
    pub fn halt(&self) {
        let mut ctrls = self.inner.task_ctrl.lock().unwrap();
        ctrls.sched.take().unwrap().exit();
        for worker in ctrls.workers.iter_mut() {
            worker.take().unwrap().exit();
        }
    }
}

struct DriverCtrls {
    sched: Option<TaskCtrl>,
    workers: Vec<Option<TaskCtrl>>,
}
impl DriverCtrls {
    fn new(sz: NonZeroUsize) -> Self {
        let mut workers = Vec::with_capacity(sz.get());
        workers.resize_with(sz.get(), Default::default);
        Self { sched: None, workers }
    }
}

struct DriverInner {
    /// The block device providing the requests to be serviced
    bdev: OnceCell<Arc<dyn Device>>,

    /// Memory accessor through the underlying device
    acc_mem: OnceCell<MemAccessor>,

    /// Queue of I/O requests from the device ready to be serviced by the backend
    queue: Mutex<VecDeque<Request>>,

    /// Sync for blocking backend worker threads on requests in the queue
    cv: Condvar,

    /// Semaphore to block polling block device unless we have idle backend worker threads
    idle_threads: Semaphore,

    /// Notify handle used by block device to proactively inform us of any new requests
    wake: Arc<Notify>,

    /// Task control handles for workers
    task_ctrl: Mutex<DriverCtrls>,
}
impl DriverInner {
    /// Worker thread's main-loop: looks for requests to service in the queue.
    fn process_requests(&self, task: &mut TaskHdl, be: Arc<dyn Backend>) {
        let mut idled = false;
        loop {
            // Heed task events
            match task.pending_event() {
                Some(Event::Hold) => {
                    task.hold();
                    continue;
                }
                Some(Event::Exit) => return,
                _ => {}
            }

            let bdev = self.bdev.get().expect("attached block device").as_ref();
            let acc_mem = self.acc_mem.get().expect("attached memory accessor");
            // Check if we've received any requests to process
            let mut guard = self.queue.lock().unwrap();
            if let Some(req) = guard.pop_front() {
                drop(guard);
                idled = false;
                match acc_mem.access() {
                    Some(mem) => {
                        let result = be.process(&req, &mem);
                        req.complete(result, bdev);
                    }
                    None => {
                        req.complete(Result::Failure, bdev);
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
                        // Be cognizant of task events in addition to the state
                        // of the queue.
                        g.is_empty() && task.pending_event().is_none()
                    })
                    .unwrap();
            }
        }
    }

    /// Attempt to grab a request from the block device (if one's available)
    async fn next_req(&self) -> Request {
        let bdev = self.bdev.get().expect("attached block device");
        loop {
            if let Some(req) = bdev.next() {
                return req;
            }

            // Don't busy-loop on the device but just wait for it to wake us
            // when the next request is available
            self.wake.notified().await;
        }
    }

    /// Scheduling task body: feed worker threads with requests from block device.
    async fn schedule_requests(&self, task: &mut TaskHdl) {
        loop {
            let avail = tokio::select! {
                event = task.get_event() => {
                    match event {
                        Event::Hold => {
                            task.wait_held().await;
                            continue;
                        }
                        Event::Exit => {
                            return;
                        }
                    }
                },
                avail = self.idle_threads.acquire() => {
                    // We found an idle thread!
                    avail.unwrap()
                }
            };

            tokio::select! {
                _event = task.get_event() => {
                    // Take another lap if an event arrives while we are waiting
                    // for a request to schedule to the worker
                    continue;
                },
                req = self.next_req() => {
                    // With a request in hand, we can discard the permit for the
                    // idle thread which is about to pick up the work.
                    avail.forget();

                    // Put the request on the queue for processing
                    let mut queue = self.queue.lock().unwrap();
                    queue.push_back(req);
                    drop(queue);
                    self.cv.notify_one();
                }
            }
        }
    }
}
