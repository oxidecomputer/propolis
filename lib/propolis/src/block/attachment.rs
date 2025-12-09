// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Block "attachments" provide the plumbing between emulated devices and the
//! backends which execute the IO requests from said devices.
//!
//! Each emulated block device will contain a [DeviceAttachment] to which it
//! will associate one or more [DeviceQueue] instances.  The queue(s) is the
//! source of [super::Request]s, which are to be processed by an attached
//! backend.
//!
//! Block backends will each contain a [BackendAttachment] which they will
//! request worker contexts from ([SyncWorkerCtx] or [AsyncWorkerCtx]).  It is
//! through the worker context that the backend will fetch [super::Request]s
//! from the associated device in order to process them.

use std::collections::BTreeMap;
use std::future::Future;
use std::marker::PhantomPinned;
use std::num::NonZeroUsize;
use std::pin::Pin;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, Weak};
use std::task::{Context, Poll};

use super::minder::{NoneInFlight, QueueMinder};
use super::{
    devq_id, probes, DeviceId, DeviceInfo, DeviceQueue, DeviceRequest,
    MetricConsumer, QueueId, WorkerId,
};
use crate::accessors::MemAccessor;

use futures::stream::FuturesUnordered;
use futures::Stream;
use pin_project_lite::pin_project;
use strum::IntoStaticStr;
use thiserror::Error;
use tokio::sync::futures::Notified;
use tokio::sync::Notify;

/// Static for generating unique block [DeviceId]s with a process
static NEXT_DEVICE_ID: AtomicU32 = AtomicU32::new(0);

pub const MAX_WORKERS: NonZeroUsize = NonZeroUsize::new(64).unwrap();

pub type ReqCountHint = Option<NonZeroUsize>;

#[derive(Default)]
struct QueueSlotState {
    minder: Option<Arc<QueueMinder>>,
}

struct QueueSlot {
    state: Mutex<QueueSlotState>,
    workers: Mutex<Option<Arc<WorkerCollection>>>,
    notify_count: AtomicUsize,
    queue_id: QueueId,
}
impl QueueSlot {
    fn new(queue_id: QueueId) -> Self {
        Self {
            state: Mutex::new(Default::default()),
            workers: Mutex::new(None),
            notify_count: AtomicUsize::new(0),
            queue_id,
        }
    }
    fn request_notify(&self, hint: ReqCountHint) {
        let existing = self.notify_count.load(Ordering::Acquire);
        if existing < MAX_WORKERS.get() {
            // wake everyone if we weren't given a hint
            let count = MAX_WORKERS.min(hint.unwrap_or(MAX_WORKERS));
            let _ = self.notify_count.fetch_add(count.get(), Ordering::Release);
        }
    }
    fn flush_notifications(&self) {
        let guard = self.workers.lock().unwrap();
        let Some(workers) = guard.as_ref() else {
            return;
        };

        let state = self.state.lock().unwrap();
        let Some(minder) = state.minder.as_ref() else {
            // The queue isn't associated with anything yet, so there are no
            // interested workers to wake.
            return;
        };

        let pending = self.notify_count.swap(0, Ordering::AcqRel);

        let Some(pending) = NonZeroUsize::new(pending) else {
            // We have not been asked to wake any workers since the last
            // `flush_notifications`. This is relatively unlikely but
            // legitimate, such as if this `QueueSlot` paused and resumed (as
            // for migrations) repeatedly.
            return;
        };

        // Take the full set of workers that may be idle and interested in this
        // queue. At this point we are responsible for either waking workers
        // here, or returning the idle-and-interested bit to `minder`.
        let Some(wake_wids) = minder.take_notifications() else {
            // `notify_count` was non-zero, but between checking the notify
            // count and getting idle workers, we started pausing devices.
            // Bummer. Request notification of as many workers as we were
            // going to, and let a future `flush_notifications()` take care of
            // it.
            self.request_notify(Some(pending));
            return;
        };
        drop(state);

        let remaining_wids =
            workers.wake(wake_wids, pending, Some(self.queue_id));

        if !remaining_wids.is_empty() {
            let state = self.state.lock().unwrap();
            let Some(minder) = state.minder.as_ref() else {
                // The queue no longer has a minder. This is unfortunate, but it
                // is at least OK to discard `remaining_wids` here: if this
                // queue is reassociated later, updating the queue collection's
                // associations will wake all queues.
                return;
            };

            minder.add_notifications(remaining_wids);
        }
    }
}

#[derive(Default, Copy, Clone)]
struct QueueColState {
    associated_qids: Versioned<Bitmap>,
    paused: bool,
}
impl QueueColState {
    fn queue_associate(&mut self, qid: QueueId) -> Versioned<Bitmap> {
        self.associated_qids.update().set(qid.into());
        self.associated_qids
    }
    fn queue_dissociate(&mut self, qid: QueueId) -> Versioned<Bitmap> {
        self.associated_qids.update().unset(qid.into());
        self.associated_qids
    }
}
struct QueueCollection {
    queues: Vec<QueueSlot>,
    state: Mutex<QueueColState>,
    pub devid: DeviceId,
}
impl QueueCollection {
    fn new(max_queues: NonZeroUsize, devid: DeviceId) -> Arc<Self> {
        let count = max_queues.get();
        assert!(count <= MAX_WORKERS.get());
        let queues =
            (0..count).map(|n| QueueSlot::new(QueueId::from(n))).collect();

        Arc::new(Self { queues, devid, state: Default::default() })
    }
    fn attach(&self, workers: &Arc<WorkerCollection>) {
        for slot in self.queues.iter() {
            let old = slot.workers.lock().unwrap().replace(workers.clone());
            assert!(old.is_none(), "workers ref should not have been attached");
        }
    }
    fn detach(&self) {
        for slot in self.queues.iter() {
            let old = slot.workers.lock().unwrap().take();
            assert!(old.is_some(), "workers ref should have been attached");
        }
    }
    fn slot(&self, queue_id: QueueId) -> &QueueSlot {
        self.queues.get(usize::from(queue_id)).expect("queue id within range")
    }
    fn notify(&self, queue_id: QueueId, hint: ReqCountHint) {
        let slot = self.slot(queue_id);
        slot.request_notify(hint);
        slot.flush_notifications();
    }
    fn set_metric_consumer(&self, consumer: Arc<dyn MetricConsumer>) {
        for queue in self.queues.iter() {
            if let Some(minder) = queue.state.lock().unwrap().minder.as_mut() {
                minder.set_metric_consumer(consumer.clone());
            }
        }
    }
    fn associated_qids(&self) -> Versioned<Bitmap> {
        self.state.lock().unwrap().associated_qids
    }
    fn pause(&self) {
        let mut state = self.state.lock().unwrap();
        assert!(!state.paused);

        state.paused = true;
        for slot in self.queues.iter() {
            if let Some(minder) = slot.state.lock().unwrap().minder.as_ref() {
                minder.pause();
            }
        }
    }
    fn resume(&self) {
        let mut state = self.state.lock().unwrap();
        assert!(state.paused);

        state.paused = false;
        for slot in self.queues.iter() {
            let state = slot.state.lock().unwrap();
            let Some(minder) = state.minder.as_ref() else {
                continue;
            };
            minder.resume();
            drop(state);

            slot.flush_notifications();
        }
    }
    fn none_processing(&self) -> NoneProcessing {
        let minders = self
            .queues
            .iter()
            .filter_map(|slot| {
                let state = slot.state.lock().unwrap();
                state.minder.as_ref().map(Arc::clone)
            })
            .collect::<Vec<_>>();
        NoneProcessing {
            minders: MinderRefs { values: minders, _pinned: PhantomPinned },
            unordered: FuturesUnordered::new(),
            loaded: false,
        }
    }

    fn next_req(
        &self,
        queue_select: QueueId,
        wid: WorkerId,
    ) -> Option<DeviceRequest> {
        let idx: usize = queue_select.into();
        let slot = self.queues.get(idx)?;

        let guard = slot.state.lock().unwrap();
        let minder = guard.minder.as_ref()?;
        let result = minder.next_req(wid);

        probes::block_poll!(|| {
            (
                devq_id(self.devid, slot.queue_id),
                wid as u64,
                result.is_some() as u8,
            )
        });
        result
    }

    fn next_req_any(
        &self,
        cursor: &mut PollCursor,
        wid: WorkerId,
    ) -> Option<DeviceRequest> {
        let idx = usize::from(cursor.0 .0);
        assert!(idx < self.queues.len());
        let (front, back) = self.queues.split_at(idx);
        let queues = back.iter().chain(front.iter());

        let (hit_qid, dreq) = queues
            .filter_map(|slot| {
                let guard = slot.state.lock().unwrap();
                let minder = guard.minder.as_ref()?;
                let result = minder.next_req(wid);

                probes::block_poll!(|| {
                    (
                        devq_id(self.devid, slot.queue_id),
                        wid as u64,
                        result.is_some() as u8,
                    )
                });

                Some((slot.queue_id, result?))
            })
            .next()?;

        // Which slot should the caller start with next time?
        cursor.0 = hit_qid.next(self.queues.len());

        Some(dreq)
    }
}

struct MinderRefs {
    values: Vec<Arc<QueueMinder>>,
    _pinned: PhantomPinned,
}
pin_project! {
    pub struct NoneProcessing {
        #[pin]
        minders: MinderRefs,
        #[pin]
        unordered: FuturesUnordered<NoneInFlight<'static>>,
        loaded: bool,
    }
    impl PinnedDrop for NoneProcessing {
        fn drop(this: Pin<&mut Self>) {
            let mut this = this.project();

            // Ensure that all references into `minders` held by NoneInFlight
            // futures are dropped before the `minders` contents themselves.
            this.unordered.clear();
        }
    }
}
impl Future for NoneProcessing {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut this = self.project();
        if !*this.loaded {
            for minder in this.minders.values.iter().map(Arc::as_ref) {
                // # SAFETY
                //
                // With the Vec<Arc<QueueMinder>> pinned (and barred via marker
                // from Unpin), it should not be possible to remove them for the
                // lifetime of this future.  With that promised to us, we can
                // extend the lifetime of the QueueMinder references long enough
                // to run the NoneInFlight futures.
                //
                // The contents of `minders` will remain pinned and untouched
                // until NoneProcessing is dropped.  At that point, any
                // lingering references held by the FuturesUnordered will be
                // explicitly released in PinnedDrop::drop(), ensuring they do
                // not outlive MinderRefs.
                let extended: &'static QueueMinder =
                    unsafe { std::mem::transmute(minder) };

                this.unordered.push(extended.none_in_flight());
            }
            *this.loaded = true;
        }
        loop {
            match Stream::poll_next(this.unordered.as_mut(), cx) {
                Poll::Ready(None) => {
                    return Poll::Ready(());
                }
                Poll::Ready(Some(_)) => {
                    continue;
                }
                Poll::Pending => {
                    return Poll::Pending;
                }
            }
        }
    }
}

/// A pair of weak references to inner state of a device and backend which are
/// attached to one another.
///
/// Attachment and detachment requires taking locks in both the device and
/// backend, and such lock is done in that specific order to avoid deadlock.
#[derive(Clone)]
struct AttachPair {
    dev_attach: Weak<DeviceAttachInner>,
    backend_attach: Weak<BackendAttachInner>,
}
impl AttachPair {
    fn attach(
        dev: &DeviceAttachment,
        be: &BackendAttachment,
    ) -> Result<(), AttachError> {
        let mut dev_att_state = dev.0.att_state.lock().unwrap();
        let mut be_att_state = be.0.att_state.lock().unwrap();

        if dev_att_state.is_some() {
            return Err(AttachError::DeviceAttached);
        }
        if be_att_state.is_some() {
            return Err(AttachError::BackendAttached);
        }

        // TODO: name the accessor child?
        let be_acc_mem = dev.0.acc_mem.child(None);
        be.0.workers.attach(&be_acc_mem, &dev.0.queues);
        dev.0.queues.attach(&be.0.workers);

        let shared = AttachPair {
            dev_attach: Arc::downgrade(&dev.0),
            backend_attach: Arc::downgrade(&be.0),
        };
        *dev_att_state = Some(shared.clone());
        *be_att_state = Some((shared, be_acc_mem));

        drop(dev_att_state);
        drop(be_att_state);

        let dev_state = dev.0.dev_state.lock().unwrap();
        if let Some(on_attach) = dev_state.on_attach.as_ref() {
            on_attach(be.info())
        }

        Ok(())
    }

    fn detach(self) {
        let (Some(dev), Some(be)) =
            (self.dev_attach.upgrade(), self.backend_attach.upgrade())
        else {
            // If the drop handler has run for the device or backend, resulting
            // in its Weak pointer being unable to upgrade, then a detach is
            // already in progress, and we can let that run to completion.
            return;
        };

        let mut dev_state = dev.att_state.lock().unwrap();
        let mut be_state = be.att_state.lock().unwrap();
        match (dev_state.as_ref(), be_state.as_ref()) {
            (Some(ds), Some((bs, _))) if self.eq(ds) && self.eq(bs) => {
                // Device and backend agree about mutual attachment
            }
            _ => {
                // It is possible for this to race with some other thread which
                // is performing detach and attach operations.  If the frontend
                // and/or backend does not match up with what we have in this
                // AttachPair, that is indicative of such a race.  Bailing out
                // here is safe, since said racing operation(s) would have
                // resulted in subsequent AttachPair-ings which maintain proper
                // references to the involved attachments.
                return;
            }
        }
        *dev_state = None;
        *be_state = None;

        // TODO: ensure workers have no in-flight requests
        be.workers.detach();
        dev.queues.detach();
    }
}
impl PartialEq for AttachPair {
    fn eq(&self, other: &Self) -> bool {
        self.dev_attach.ptr_eq(&other.dev_attach)
            && self.backend_attach.ptr_eq(&other.backend_attach)
    }
}

pub type OnAttachFn = Box<dyn Fn(DeviceInfo) + Send + Sync + 'static>;

#[derive(Default)]
struct DeviceState {
    on_attach: Option<OnAttachFn>,
}

struct DeviceAttachInner {
    att_state: Mutex<Option<AttachPair>>,
    dev_state: Mutex<DeviceState>,
    queues: Arc<QueueCollection>,
    acc_mem: MemAccessor,
}

/// Main "attachment point" for a block device.
pub struct DeviceAttachment(Arc<DeviceAttachInner>);
impl DeviceAttachment {
    /// Create a [DeviceAttachment] for a given device.  The maximum number of
    /// queues which the device will ever expose is set via `max_queues`.  DMA
    /// done by attached backend workers will be through the provided `acc_mem`.
    pub fn new(max_queues: NonZeroUsize, acc_mem: MemAccessor) -> Self {
        let devid = NEXT_DEVICE_ID.fetch_add(1, Ordering::Relaxed);
        let queues = QueueCollection::new(max_queues, devid);
        Self(Arc::new(DeviceAttachInner {
            att_state: Mutex::new(None),
            dev_state: Mutex::new(DeviceState::default()),
            queues,
            acc_mem,
        }))
    }

    /// If a backend is attached to this device, notify it that the queue
    /// associations for this device have changed.
    fn queues_update_assoc(&self, queues_associated: Versioned<Bitmap>) {
        let guard = self.0.att_state.lock().unwrap();
        if let Some(att_state) = guard.as_ref() {
            if let Some(backend) = Weak::upgrade(&att_state.backend_attach) {
                drop(guard);
                backend.workers.update_queue_associations(queues_associated);
            }
        }
    }

    /// Associate a [DeviceQueue] with this device.
    ///
    /// Once associated, any attached backend will process requests emitted from
    /// that queue.
    ///
    /// # Panics
    ///
    /// If `queue_id` is >= the max queues specified for this device, or if
    /// an existing queue is associated with that ID.
    pub fn queue_associate(
        &self,
        queue_id: QueueId,
        queue: Arc<impl DeviceQueue>,
    ) {
        let minder = QueueMinder::new(queue, self.0.queues.devid, queue_id);

        let mut state = self.0.queues.state.lock().unwrap();
        let slot = self.0.queues.slot(queue_id);
        let mut slot_state = slot.state.lock().unwrap();
        assert!(
            slot_state.minder.is_none(),
            "queue slot should not be occupied"
        );

        if state.paused {
            // Propagate any pause state of the device into any newly
            // associating queues while in such a pause.
            minder.pause();
        }
        slot_state.minder = Some(minder);
        drop(slot_state);

        let associated = state.queue_associate(queue_id);
        drop(state);

        self.queues_update_assoc(associated);
    }

    /// Dissociate a [DeviceQueue] from this device
    ///
    /// After dissociation, any attached backend will cease processing requests
    /// from that queue.
    ///
    /// # Panics
    ///
    /// if `queue_id` is >= the max queues specified for this device, or if
    /// there is not queue associated with that ID.
    pub fn queue_dissociate(&self, queue_id: QueueId) {
        let mut state = self.0.queues.state.lock().unwrap();
        let slot = self.0.queues.slot(queue_id);
        let mut slot_state = slot.state.lock().unwrap();

        let minder =
            slot_state.minder.take().expect("queue slot should be occupied");
        minder.destroy();
        drop(slot_state);

        let associated = state.queue_dissociate(queue_id);
        drop(state);

        self.queues_update_assoc(associated);
    }

    /// Notify attached backend (if any) that `queue_id` may have new IO
    /// requests to process.  If the number of available requests is known, it
    /// can be communicated via `hint` in order to optimize worker waking.
    pub fn notify(&self, queue_id: QueueId, hint: ReqCountHint) {
        self.0.queues.notify(queue_id, hint);
    }

    pub fn device_id(&self) -> DeviceId {
        self.0.queues.devid
    }
    /// Get the maximum queues configured for this device.
    pub fn max_queues(&self) -> NonZeroUsize {
        NonZeroUsize::new(self.0.queues.queues.len())
            .expect("non-zero queue count")
    }
    /// Get the [DeviceInfo] of the attached backend (if any)
    pub fn info(&self) -> Option<DeviceInfo> {
        let state = self.0.att_state.lock().unwrap();
        let backend = Weak::upgrade(&state.as_ref()?.backend_attach)?;
        Some(backend.info)
    }

    /// Detach the device from attached backend (if any)
    pub fn detach(&self) {
        let guard = self.0.att_state.lock().unwrap();
        if let Some(att_state) = guard.as_ref().map(Clone::clone) {
            drop(guard);
            att_state.detach();
        }
    }

    /// Pause the device, preventing workers from an attached backend (if any)
    /// from fetching new IO requests to process.  Outstanding requests will
    /// proceed as normal.
    pub fn pause(&self) {
        self.0.queues.pause();
    }

    /// Resume the device, allowing workers from an attached backend (if any) to
    /// once again fetch new IO requests to process.
    pub fn resume(&self) {
        self.0.queues.resume();
    }

    /// Emit a [Future] which will resolve when there are no request being
    /// actively processed by an attached backend.
    pub fn none_processing(&self) -> NoneProcessing {
        self.0.queues.none_processing()
    }

    /// Set the [MetricConsumer] to be informed of all request completions
    /// processed by this device.
    pub fn set_metric_consumer(&self, consumer: Arc<dyn MetricConsumer>) {
        self.0.queues.set_metric_consumer(consumer);
    }

    /// Register a function to be called when this device becomes attached to a
    /// backend.  Intended for tasks such as querying the [DeviceInfo] for
    /// presentation to the guest.
    pub fn on_attach(&self, cb: OnAttachFn) {
        self.0.dev_state.lock().unwrap().on_attach = Some(cb);
    }
}
impl Drop for DeviceAttachment {
    fn drop(&mut self) {
        self.detach();
    }
}

#[derive(Copy, Clone, Default)]
struct PollCursor(QueueId);
impl PollCursor {
    /// Suggest that the worker using this cursor poll a specific queue next
    fn suggest(&mut self, queue_id: QueueId) {
        self.0 = queue_id;
    }
}

#[derive(Default)]
struct WorkerState {
    /// Has the worker associated with this slot indicated that it is active?
    active_type: Option<WorkerType>,
    /// Has the absence of work caused this worker to sleep?
    sleeping_on: Option<DeviceId>,

    assign_strat: Versioned<Strategy>,
    assign_poll: PollAssignment,

    cursor: PollCursor,

    queues: Option<Arc<QueueCollection>>,
}

pub(crate) struct WorkerSlot {
    state: Mutex<WorkerState>,
    acc_mem: MemAccessor,
    cv: Condvar,
    notify: Notify,
    id: WorkerId,
}
impl WorkerSlot {
    fn new(id: WorkerId) -> Self {
        Self {
            state: Mutex::new(Default::default()),
            acc_mem: MemAccessor::new_orphan(),
            cv: Condvar::new(),
            notify: Notify::new(),
            id,
        }
    }
    fn block_for_req(&self) -> Option<DeviceRequest> {
        let mut state = self.state.lock().unwrap();
        assert!(state.active_type.is_some());

        loop {
            let devid = match self.next_req(&mut state) {
                PollResult::Ok(device_request) => {
                    return Some(device_request);
                }
                PollResult::Detached | PollResult::Halted => {
                    return None;
                }
                PollResult::WaitFor(devid) => devid,
            };

            state.sleeping_on = Some(devid);
            probes::block_sleep!(|| { (devid, self.id as u64) });
            state = self.cv.wait(state).unwrap();
            probes::block_wake!(|| { (devid, self.id as u64) });
            state.sleeping_on = None;
        }
    }

    fn next_req(&self, state: &mut MutexGuard<WorkerState>) -> PollResult {
        assert!(state.active_type.is_some());

        let Some(queues) = state.queues.as_ref() else {
            return PollResult::Detached;
        };
        let devid = queues.devid;
        let result = match state.assign_poll {
            PollAssignment::Halt => {
                return PollResult::Halted;
            }
            PollAssignment::Idle => None,
            PollAssignment::Fixed(queue_id) => {
                queues.next_req(queue_id, self.id)
            }
            PollAssignment::Any => {
                // Copy cursor since split borrows confuses borrowck
                let mut cursor = state.cursor;
                let result = queues.next_req_any(&mut cursor, self.id);
                state.cursor = cursor;
                result
            }
        };
        match result {
            Some(req) => PollResult::Ok(req),
            None => PollResult::WaitFor(devid),
        }
    }

    fn async_start_sleep(
        &self,
        mut state: MutexGuard<WorkerState>,
        devid: DeviceId,
    ) {
        state.sleeping_on = Some(devid);
        probes::block_sleep!(|| { (devid, self.id as u64) });
    }

    fn async_stop_sleep(&self) {
        let mut state = self.state.lock().unwrap();
        if let Some(devid) = state.sleeping_on.take() {
            probes::block_wake!(|| { (devid, self.id as u64) });
        }
    }

    fn wait_for_req(&self) -> WaitForReq<'_> {
        WaitForReq::new(self)
    }

    fn update_assignment(&self, assign: &Assignment) {
        let mut state = self.state.lock().unwrap();
        if state.assign_strat.newer_than(&assign.strategy) {
            // We already have a newer assignment
            return;
        }
        state.assign_strat = assign.strategy;
        if assign.should_halt {
            state.assign_poll = PollAssignment::Halt;
        } else {
            state.assign_poll =
                if let Some(poll_assign) = assign.poll_assignments.as_ref() {
                    *poll_assign.get(&self.id).unwrap_or(&PollAssignment::Any)
                } else {
                    PollAssignment::Idle
                };
        }
        self.wake(Some(state), None);
    }

    fn wake(
        &self,
        state: Option<MutexGuard<WorkerState>>,
        qid_hint: Option<QueueId>,
    ) -> bool {
        let mut state = state.unwrap_or_else(|| self.state.lock().unwrap());
        if let Some(wtype) = state.active_type {
            if state.sleeping_on.is_some() {
                if let Some(qid) = qid_hint {
                    state.cursor.suggest(qid);
                }
                match wtype {
                    WorkerType::Sync => self.cv.notify_one(),
                    WorkerType::Async => self.notify.notify_one(),
                }
                return true;
            }
        }

        false
    }
}

/// Device queue worker is assigned to poll
#[derive(Clone, Copy, Default)]
enum PollAssignment {
    /// End polling immediately since backend is halted
    Halt,
    /// Poll no queue(s) as worker is in idle state
    Idle,
    /// Fixed queue specified by [QueueId]
    Fixed(QueueId),
    /// Poll any queue(s)
    #[default]
    Any,
}

#[derive(Default)]
struct WorkerColState {
    backend_running: bool,

    strategy: Versioned<Strategy>,

    workers_active: Bitmap,

    associated_qids: Versioned<Bitmap>,

    device_id: Option<DeviceId>,
}
impl WorkerColState {
    fn set_worker_state(&mut self, wid: WorkerId, is_active: bool) {
        if is_active {
            self.workers_active.set(wid);
        } else {
            self.workers_active.unset(wid);
        }
    }
    /// Based on active workers and queues, pick a suitable dispatch strategy
    /// and perform any worker->queue assignments (if applicable to the newly
    /// selected strategy).
    fn generate_assignments(&mut self) -> Assignment {
        // Pick a (potentially) new strategy in the face of updated state
        self.strategy.replace(if self.backend_running {
            Strategy::choose(
                self.workers_active.count(),
                self.associated_qids.get().count(),
            )
        } else {
            Strategy::Idle
        });

        if !self.backend_running {
            return Assignment {
                strategy: self.strategy,
                poll_assignments: None,
                should_halt: true,
            };
        }
        let poll_assignments = match self.strategy.get() {
            Strategy::Idle => None,
            Strategy::Single => {
                assert_eq!(self.associated_qids.get().count(), 1);
                let single_queue: QueueId =
                    self.associated_qids.get().iter().next().unwrap().into();

                Some(
                    self.workers_active
                        .iter()
                        .map(|wid| (wid, PollAssignment::Fixed(single_queue)))
                        .collect(),
                )
            }
            Strategy::Static => {
                let worker_count = self.workers_active.count();
                let queue_count = self.associated_qids.get().count();
                assert!(
                    worker_count >= queue_count,
                    "workers should >= queues when {:?} is chosen",
                    self.strategy.get()
                );
                let per_queue = worker_count / queue_count;
                let mut queue_loop = self.associated_qids.get().looping_iter();

                let mut workers = self.workers_active.iter();

                let mut assigned: BTreeMap<WorkerId, PollAssignment> = workers
                    .by_ref()
                    .take(per_queue * queue_count)
                    .map(|wid| {
                        (
                            wid,
                            PollAssignment::Fixed(queue_loop.next().expect(
                                "looping queue iter should emit results",
                            ).into()),
                        )
                    })
                    .collect();
                // Remaining workers will be idled
                assigned.extend(workers.map(|wid| (wid, PollAssignment::Idle)));

                Some(assigned)
            }
            Strategy::FreeForAll => Some(
                self.workers_active
                    .iter()
                    .map(|wid| (wid, PollAssignment::Any))
                    .collect(),
            ),
        };
        Assignment {
            strategy: self.strategy,
            poll_assignments,
            should_halt: false,
        }
    }
}

#[derive(Default, Copy, Clone, PartialEq, Eq, Debug, IntoStaticStr)]
pub enum Strategy {
    /// An explicitly stopped backend or lack of workers and/or queues means
    /// there is no dispatching to do
    #[default]
    Idle,

    /// All workers servicing single queue
    Single,

    /// Workers are statically assigned to queues in an even distribution.
    Static,

    /// Workers will round-robin through all queues, attempting to pick up
    /// requests from any they can.
    FreeForAll,
}
impl Strategy {
    pub fn choose(worker_count: usize, queue_count: usize) -> Self {
        if worker_count == 0 || queue_count == 0 {
            return Strategy::Idle;
        }
        if queue_count == 1 {
            return Strategy::Single;
        }
        if worker_count >= queue_count {
            return Strategy::Static;
        }
        // Unfortunate, but better than leaving requests to linger in a queue
        // which lacks any assigned workers
        Strategy::FreeForAll
    }
}

struct Assignment {
    strategy: Versioned<Strategy>,
    poll_assignments: Option<BTreeMap<WorkerId, PollAssignment>>,
    should_halt: bool,
}

pub(crate) struct WorkerCollection {
    workers: Vec<WorkerSlot>,
    state: Mutex<WorkerColState>,
}
impl WorkerCollection {
    fn new(max_workers: NonZeroUsize) -> Arc<Self> {
        let max_workers = max_workers.get();
        assert!(max_workers <= MAX_WORKERS.get());
        let workers: Vec<_> = (0..max_workers)
            .map(|id| WorkerSlot::new(WorkerId::from(id)))
            .collect();
        Arc::new(Self { workers, state: Default::default() })
    }
    fn set_active(&self, id: WorkerId, new_type: Option<WorkerType>) -> bool {
        if let Some(slot) = self.workers.get(id) {
            let refresh_guard = {
                let mut wstate = slot.state.lock().unwrap();
                if wstate.active_type.is_some() != new_type.is_some() {
                    let mut cstate = self.state.lock().unwrap();
                    cstate.set_worker_state(id, new_type.is_some());
                    wstate.active_type = new_type;
                    Some(cstate)
                } else {
                    None
                }
            };

            if let Some(guard) = refresh_guard {
                self.assignments_refresh(guard);
                return true;
            }
        }
        false
    }
    fn assignments_refresh(&self, mut state: MutexGuard<WorkerColState>) {
        let assign = state.generate_assignments();
        let devid = state.device_id.unwrap_or(u32::MAX);
        drop(state);

        super::probes::block_strategy!(|| {
            let assign_name: &'static str = assign.strategy.get().into();
            let generation = assign.strategy.generation() as u64;
            (devid, assign_name, generation)
        });
        for slot in self.workers.iter() {
            slot.update_assignment(&assign);
        }
    }
    fn slot(&self, id: WorkerId) -> &WorkerSlot {
        self.workers.get(id).expect("valid worker id for slot")
    }
    fn attach(&self, parent_mem: &MemAccessor, queues: &Arc<QueueCollection>) {
        for (idx, slot) in self.workers.iter().enumerate() {
            parent_mem.adopt(&slot.acc_mem, Some(format!("worker-{idx}")));
            let mut state = slot.state.lock().unwrap();
            let old = state.queues.replace(queues.clone());
            assert!(old.is_none(), "worker slot not already attached");
        }

        let mut state = self.state.lock().unwrap();
        state.device_id = Some(queues.devid);
        state.associated_qids = queues.associated_qids();
    }
    fn detach(&self) {
        for slot in self.workers.iter() {
            let mut state = slot.state.lock().unwrap();
            let old = state.queues.take();
            assert!(old.is_some(), "worker slot should have been attached");
        }
        let mut state = self.state.lock().unwrap();
        state.strategy.replace(Strategy::Idle);
        // With no device attached, the queues information should be cleared
        state.associated_qids = Versioned::default();
        state.device_id = None;
    }
    fn wake(
        &self,
        wake_wids: Bitmap,
        limit: NonZeroUsize,
        qid_hint: Option<QueueId>,
    ) -> Bitmap {
        probes::block_worker_collection_wake!(|| (wake_wids.0, limit.get()));

        let mut num_woken = 0;
        let mut idle_wids = wake_wids.iter();

        for wid in &mut idle_wids {
            let Some(slot) = self.workers.get(wid) else {
                continue;
            };

            if slot.wake(None, qid_hint) {
                num_woken += 1;
            }

            if num_woken == limit.get() {
                break;
            }
        }

        let remainder = idle_wids.remainder();

        probes::block_worker_collection_woken!(|| (remainder.0, num_woken));

        remainder
    }
    fn update_queue_associations(&self, queues_associated: Versioned<Bitmap>) {
        let mut state = self.state.lock().unwrap();
        state.associated_qids.replace_if_newer(&queues_associated);
        self.assignments_refresh(state);
    }
    fn start(&self) {
        let mut state = self.state.lock().unwrap();
        state.backend_running = true;
        self.assignments_refresh(state);
    }
    fn stop(&self) {
        let mut state = self.state.lock().unwrap();
        state.backend_running = false;
        self.assignments_refresh(state);
    }
}

#[derive(Copy, Clone)]
pub enum WorkerType {
    Sync,
    Async,
}

pub struct InactiveWorkerCtx {
    workers: Arc<WorkerCollection>,
    id: WorkerId,
}
impl InactiveWorkerCtx {
    /// Activate this worker for synchronous operation.
    ///
    /// Returns [None] if there is already an active worker in the slot
    /// associated with this [WorkerId].
    pub fn activate_sync(self) -> Option<SyncWorkerCtx> {
        if self.workers.set_active(self.id, Some(WorkerType::Sync)) {
            Some(SyncWorkerCtx(self.into()))
        } else {
            None
        }
    }

    /// Activate this worker for asynchronous operation.
    ///
    /// Returns [None] if there is already an active worker in the slot
    /// associated with this [WorkerId].
    pub fn activate_async(self) -> Option<AsyncWorkerCtx> {
        if self.workers.set_active(self.id, Some(WorkerType::Async)) {
            Some(AsyncWorkerCtx(self.into()))
        } else {
            None
        }
    }
}

/// Worker context for synchronous (blocking) request processing.
///
/// Note: When the context is dropped, the slot for this [WorkerId] will become
/// vacant, and available to be activated again.
pub struct SyncWorkerCtx(WorkerCtxInner);
impl SyncWorkerCtx {
    /// Block (synchronously) in order to retrieve the next
    /// [request](DeviceRequest) from the device.  Will return [None] if no
    /// device is attached, or the backend is stopped, otherwise it will block
    /// until a request is available.
    pub fn block_for_req(&self) -> Option<DeviceRequest> {
        self.0.workers.slot(self.0.id).block_for_req()
    }
    /// Get the [MemAccessor] required to do DMA for request processing
    pub fn acc_mem(&self) -> &MemAccessor {
        self.0.acc_mem()
    }
}

/// Worker context for asynchronous request processing
///
/// Note: When the context is dropped, the slot for this [WorkerId] will become
/// vacant, and available to be activated again.
pub struct AsyncWorkerCtx(WorkerCtxInner);
impl AsyncWorkerCtx {
    /// Get a [Future] which will wait for a [request](DeviceRequest) to be made
    /// available from an attached device.
    pub fn wait_for_req(&self) -> WaitForReq<'_> {
        self.0.workers.slot(self.0.id).wait_for_req()
    }
    /// Get the [MemAccessor] required to do DMA for request processing
    pub fn acc_mem(&self) -> &MemAccessor {
        self.0.acc_mem()
    }
}

struct WorkerCtxInner {
    workers: Arc<WorkerCollection>,
    id: WorkerId,
}
impl From<InactiveWorkerCtx> for WorkerCtxInner {
    fn from(value: InactiveWorkerCtx) -> Self {
        let InactiveWorkerCtx { workers, id } = value;
        WorkerCtxInner { workers, id }
    }
}
impl WorkerCtxInner {
    fn acc_mem(&self) -> &MemAccessor {
        &self.workers.slot(self.id).acc_mem
    }
}
impl Drop for WorkerCtxInner {
    /// Deactivate the worker when it is dropped
    fn drop(&mut self) {
        assert!(
            self.workers.set_active(self.id, None),
            "active worker is valid during deactivation"
        );
    }
}

struct BackendAttachInner {
    att_state: Mutex<Option<(AttachPair, MemAccessor)>>,
    workers: Arc<WorkerCollection>,
    info: DeviceInfo,
}

/// Main "attachment point" for a block backend.
pub struct BackendAttachment(Arc<BackendAttachInner>);
impl BackendAttachment {
    pub fn new(max_workers: NonZeroUsize, info: DeviceInfo) -> Self {
        Self(Arc::new(BackendAttachInner {
            att_state: Mutex::new(None),
            workers: WorkerCollection::new(max_workers),
            info,
        }))
    }
    /// Get an (inactive) [context](InactiveWorkerCtx) for a given [WorkerId].
    pub fn worker(&self, id: WorkerId) -> InactiveWorkerCtx {
        assert!(id < self.0.workers.workers.len());
        InactiveWorkerCtx { workers: self.0.workers.clone(), id }
    }

    pub fn max_workers(&self) -> NonZeroUsize {
        NonZeroUsize::new(self.0.workers.workers.len())
            .expect("WorkerCollection correctly initialized")
    }

    pub fn info(&self) -> DeviceInfo {
        self.0.info
    }

    /// Permit workers to pull requests from the attached device (if any) for
    /// processing.
    pub fn start(&self) {
        self.0.workers.start()
    }

    /// Remove access to pull requests from the attached device (if any) from
    /// workers, causing them to halt processing once they have completed any
    /// in-flight work.
    pub fn stop(&self) {
        self.0.workers.stop()
    }

    /// Detach this backend from the device (if any)
    pub fn detach(&self) {
        let guard = self.0.att_state.lock().unwrap();
        if let Some(att_state) =
            guard.as_ref().map(|(att_state, _)| att_state.clone())
        {
            drop(guard);
            att_state.detach();
        }
    }
}
impl Drop for BackendAttachment {
    fn drop(&mut self) {
        self.detach()
    }
}

/// Attach a [device](DeviceAttachment) to a [backend](BackendAttachment).
pub fn attach(
    device: &DeviceAttachment,
    backend: &BackendAttachment,
) -> Result<(), AttachError> {
    AttachPair::attach(device, backend)
}

pin_project! {
    /// [Future] returned from [`Waiter::for_req()`]
    pub struct WaitForReq<'a> {
        slot: &'a WorkerSlot,
        sleeping_on: Option<DeviceId>,
        #[pin]
        wait: Notified<'a>
    }

    impl PinnedDrop for WaitForReq<'_> {
        fn drop(this: Pin<&mut Self>) {
            let this = this.project();
            if let Some(_) = this.sleeping_on.take() {
                this.slot.async_stop_sleep();
            }
        }
    }
}

impl WaitForReq<'_> {
    fn new<'a>(slot: &'a WorkerSlot) -> WaitForReq<'a> {
        let wait = slot.notify.notified();
        WaitForReq { slot, sleeping_on: None, wait }
    }
}

impl Future for WaitForReq<'_> {
    type Output = Option<DeviceRequest>;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut this = self.project();

        if let Some(_) = this.sleeping_on.take() {
            this.slot.async_stop_sleep();
        }

        loop {
            let mut state = this.slot.state.lock().unwrap();
            match this.slot.next_req(&mut state) {
                PollResult::Ok(dreq) => {
                    return Poll::Ready(Some(dreq));
                }
                PollResult::WaitFor(devid) => {
                    // Record that this worker is going to sleep
                    *this.sleeping_on = Some(devid);
                    this.slot.async_start_sleep(state, devid);

                    if let Poll::Ready(_) =
                        Notified::poll(this.wait.as_mut(), cx)
                    {
                        // The `Notified` future is fused, so we must "refresh"
                        // prior to any subsequent attempts to poll it after it
                        // emits `Ready`
                        this.wait.set(this.slot.notify.notified());

                        // Take another lap if woken by the notifier to check
                        // for a pending request
                        continue;
                    }
                    return Poll::Pending;
                }
                PollResult::Detached | PollResult::Halted => {
                    return Poll::Ready(None);
                }
            }
        }
    }
}

enum PollResult {
    /// Accepted request from device queue
    Ok(DeviceRequest),
    /// Worker has been idled, likely due to empty device queue(s)
    WaitFor(DeviceId),
    /// Backend is not attached to any device
    Detached,
    /// Backend is halting workers
    Halted,
}

#[derive(Error, Debug)]
pub enum AttachError {
    #[error("backend already attached")]
    BackendAttached,
    #[error("device already attached")]
    DeviceAttached,
}

/// Resource versioned with a generation number
#[derive(Copy, Clone)]
struct Versioned<T: Copy + Clone> {
    generation: usize,
    item: T,
}
impl<T: Copy + Clone> Versioned<T> {
    fn new(item: T) -> Self {
        Self { generation: 0, item }
    }
    /// Is this resource newer than `compare`?
    fn newer_than(&self, compare: &Self) -> bool {
        self.generation > compare.generation
    }
    /// Get mutable reference to resource while incrementing its generation
    fn update(&mut self) -> &mut T {
        self.generation += 1;
        &mut self.item
    }
    /// Replace contained resource and increment the generation
    fn replace(&mut self, item: T) {
        *self.update() = item;
    }
    fn replace_if_newer(&mut self, compare: &Self) {
        if compare.newer_than(self) {
            *self = *compare;
        }
    }
    fn get(&self) -> T {
        self.item
    }
    fn generation(&self) -> usize {
        self.generation
    }
}
impl<T: Copy + Clone + Default> Default for Versioned<T> {
    fn default() -> Self {
        Self::new(T::default())
    }
}

/// Simple bitmap which facilitates iterator over bits which are asserted
#[derive(Copy, Clone, Default)]
pub(crate) struct Bitmap(u64);
impl Bitmap {
    const TOP_BIT: usize = u64::BITS as usize;

    pub const ALL: Self = Self(u64::MAX);

    pub fn set(&mut self, idx: usize) {
        assert!(idx < Self::TOP_BIT);
        self.0 |= 1u64 << idx;
    }
    pub fn unset(&mut self, idx: usize) {
        assert!(idx < Self::TOP_BIT);
        self.0 &= !(1u64 << idx);
    }
    pub fn set_all(&mut self, other: Bitmap) {
        self.0 |= other.0;
    }
    pub fn lowest_set(&self) -> Option<usize> {
        if self.0.count_ones() == 0 {
            None
        } else {
            Some(self.0.trailing_zeros() as usize)
        }
    }
    pub fn count(&self) -> usize {
        self.0.count_ones() as usize
    }
    pub fn is_empty(&self) -> bool {
        self.count() == 0
    }
    pub fn take(&mut self) -> Self {
        Self(std::mem::replace(&mut self.0, 0))
    }
    /// Get iterator which emits indices of bits which are set in this map.
    pub fn iter(&self) -> BitIter {
        BitIter(*self)
    }
    /// Get iterator which emits indices of bits which are set in this map.
    /// It will infinitely loop back to the first bit whenever the last bit is
    /// reached.
    pub fn looping_iter(&self) -> LoopIter {
        LoopIter { orig: *self, cur: *self }
    }
}

pub struct BitIter(Bitmap);
impl Iterator for BitIter {
    type Item = usize;

    fn next(&mut self) -> Option<Self::Item> {
        let idx = self.0.lowest_set()?;
        self.0.unset(idx);
        Some(idx)
    }
}
impl BitIter {
    fn remainder(self) -> Bitmap {
        self.0
    }
}
pub struct LoopIter {
    cur: Bitmap,
    orig: Bitmap,
}
impl Iterator for LoopIter {
    type Item = usize;

    fn next(&mut self) -> Option<Self::Item> {
        if self.orig.count() == 0 {
            return None;
        }
        if self.cur.count() == 0 {
            self.cur = self.orig;
        }
        let idx = self.cur.lowest_set().unwrap();
        self.cur.unset(idx);
        Some(idx)
    }
}
