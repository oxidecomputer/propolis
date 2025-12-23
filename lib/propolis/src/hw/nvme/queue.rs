// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::collections::{HashMap, HashSet};
use std::fmt::{self, Debug};
use std::sync::atomic::{fence, AtomicU16, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};

use super::bits::{CompletionQueueEntry, SubmissionQueueEntry};
use super::cmds::Completion;
use crate::accessors::MemAccessor;
use crate::block;
use crate::common::*;
use crate::hw::nvme::DeviceId;
use crate::hw::pci;
use crate::migrate::MigrateStateError;
use crate::vmm::MemCtx;

use thiserror::Error;

#[usdt::provider(provider = "propolis")]
mod probes {
    fn nvme_cqe(devcq_id: u64, idx: u16, phase: u8) {}
    fn nvme_sq_dbbuf_read(devsq_id: u64, val: u32, tail: u16) {}
    fn nvme_sq_dbbuf_write(devsq_id: u64, head: u16) {}
    fn nvme_sq_dbbuf_write_shadow(devsq_id: u64, head: u16) {}
    fn nvme_cq_dbbuf_read(devcq_id: u64, val: u32, tail: u16) {}
    fn nvme_cq_dbbuf_write(devcq_id: u64, head: u16) {}
    fn nvme_cq_dbbuf_write_shadow(devcq_id: u64, head: u16) {}
}

/// Each queue is identified by a 16-bit ID.
///
/// See NVMe 1.0e Section 4.1.4 Queue Identifier
///
/// Submission and completion queue IDs are distinct namespaces, so a device
/// can have both a "Submission Queue 1" and "Completion Queue 1".
///
/// For USDT probes, we combine this ID with an NVMe controller ID to produce a
/// `devq_id`. This combined identifier is still ambiguous beteen one submission
/// queue and one completion queue. Contextually there is typically only one
/// reasonable interpretation of the ID, but the probe arguments are named
/// `devsq_id` or `devcq_id` to be explicit about identifying a Submission or
/// Completion queue, respectively.
pub type QueueId = u16;

/// The minimum number of entries in either a Completion or Submission Queue.
///
/// Note: One entry will always be unavailable for use due to the Head and Tail
///       entry pointer definitions.
/// See NVMe 1.0e Section 4.1.3 Queue Size
const MIN_QUEUE_SIZE: u32 = 2;

/// The maximum number of entries in either a Completion or Submission Queue.
///
/// See NVMe 1.0e Section 4.1.3 Queue Size
pub const MAX_QUEUE_SIZE: u32 = 1 << 16;

/// The maximum number of entries in the Admin Completion or Admin Submission Queues.
///
/// See NVMe 1.0e Section 4.1.3 Queue Size
const MAX_ADMIN_QUEUE_SIZE: u32 = 1 << 12;

/// The Admin Completion and Submission are defined to have ID 0.
///
/// See NVMe 1.0e Section 1.6.1 Admin Queue
pub const ADMIN_QUEUE_ID: QueueId = 0;

/// Completion Queue State
struct CompQueueState {
    /// Number of entries that are available for use.
    ///
    /// Starts off as queue size - 1 and gets decremented for each corresponding
    /// Submission Queue entry we begin servicing.
    avail: u16,

    /// The current phase tag.
    ///
    /// The Phase Tag is used to identify to the host (VM) that a Completion
    /// entry is new. Flips every time the Tail entry pointer wraps around.
    ///
    /// See NVMe 1.0e Section 4.5 Completion Queue Entry - Phase Tag (P)
    phase: bool,

    /// Collection of Submission Queues (IDs) which issue their completions to
    /// this CQ, and which became "corked": unable to acquire a permit while
    /// attempting to process a pending entry.
    corked: HashSet<QueueId>,
}

/// Submission Queue State
struct SubQueueState {
    params: TransferParams,
}

/// Helper for manipulating Completion/Submission Queues
///
/// The type parameter `QS` is used to constrain the set of methods exposed
/// based on whether the queue in question is a Completion or Submission queue.
///
/// Use either [CompQueueState] or [SubQueueState].
struct QueueState<QS> {
    /// The size of the queue in question.
    ///
    /// See NVMe 1.0e Section 4.1.3 Queue Size
    size: u32,

    /// The actual queue state that is updated during the normal course of
    /// operation.
    ///
    /// Either `CompQueueState` for a Completion Queue or
    /// a `SubQueueState` for a Submission Queue.
    inner: Mutex<QueueInner<QS>>,

    pub acc_mem: MemAccessor,
}
impl<QS> QueueState<QS> {
    fn new(size: u32, acc_mem: MemAccessor, inner: QS) -> Self {
        assert!(size >= MIN_QUEUE_SIZE && size <= MAX_QUEUE_SIZE);
        Self {
            size,
            inner: Mutex::new(QueueInner {
                head: 0,
                tail: 0,
                db_buf: None,
                inner,
            }),
            acc_mem,
        }
    }
    fn lock(&self) -> QueueGuard<'_, QS> {
        QueueGuard {
            size: &self.size,
            acc_mem: &self.acc_mem,
            state: self.inner.lock().unwrap(),
        }
    }
}

fn wrap_add(size: u32, idx: u16, off: u16) -> u16 {
    debug_assert!(u32::from(idx) < size);
    debug_assert!(u32::from(off) < size);

    let res = u32::from(idx) + u32::from(off);
    if res >= size {
        (res - size) as u16
    } else {
        res as u16
    }
}
fn wrap_sub(size: u32, idx: u16, off: u16) -> u16 {
    debug_assert!(u32::from(idx) < size);
    debug_assert!(u32::from(off) < size);

    if off > idx {
        ((u32::from(idx) + size) - u32::from(off)) as u16
    } else {
        idx - off
    }
}

/// Validates whether the given parameters may be used to create an SQ/CQ
fn validate(
    is_admin_queue: bool,
    base: GuestAddr,
    size: u32,
) -> Result<(), QueueCreateErr> {
    if (base.0 & PAGE_OFFSET as u64) != 0 {
        return Err(QueueCreateErr::InvalidBaseAddr);
    }
    let max =
        if is_admin_queue { MAX_ADMIN_QUEUE_SIZE } else { MAX_QUEUE_SIZE };
    if size < MIN_QUEUE_SIZE || size > max {
        return Err(QueueCreateErr::InvalidSize);
    }
    Ok(())
}

struct QueueInner<QS> {
    /// The Queue Head entry pointer.
    ///
    /// The consumer of entries on a queue uses the current Head entry pointer
    /// to identify the next entry to be pulled off the queue.
    ///
    /// See NVMe 1.0e Section 4.1 Submission Queue & Completion Queue Definition
    head: u16,

    /// The Queue Tail entry pointer.
    ///
    /// The submitter of entries to a queue uses the current Tail entry pointer
    /// to identify the next open queue entry space.
    ///
    /// See NVMe 1.0e Section 4.1 Submission Queue & Completion Queue Definition
    tail: u16,

    /// Doorbell Buffer for assisting in elision of doorbell ringing
    db_buf: Option<DoorbellBuffer>,

    /// Additional state specific to the queue type (completion or submission)
    inner: QS,
}

struct QueueGuard<'a, QS> {
    state: MutexGuard<'a, QueueInner<QS>>,
    size: &'a u32,
    acc_mem: &'a MemAccessor,
}
impl<QS> QueueGuard<'_, QS> {
    /// Returns if the queue is currently empty with the given head and tail
    /// pointers.
    ///
    /// A queue is empty when the Head entry pointer equals the Tail entry
    /// pointer.
    ///
    /// See: NVMe 1.0e Section 4.1.1 Empty Queue
    fn is_empty(&self) -> bool {
        self.state.head == self.state.tail
    }

    /// Returns if the queue is currently full with the given head and tail
    /// pointers.
    ///
    /// The queue is full when the Head entry pointer equals one more than the
    /// Tail entry pointer. The number of entries in a queue will always be 1
    /// less than the queue size.
    ///
    /// See: NVMe 1.0e Section 4.1.2 Full Queue
    fn is_full(&self) -> bool {
        let state = &self.state;

        (state.head > 0 && state.tail == (state.head - 1))
            || (state.head == 0 && state.tail == (*self.size - 1) as u16)
    }

    /// How many queue entries are currently occupied?
    fn num_occupied(&self) -> u16 {
        wrap_sub(*self.size, self.state.tail, self.state.head)
    }

    /// Helper method to calculate a positive offset for a given index, wrapping
    /// at the size of the queue.
    fn idx_add(&self, idx: u16, off: u16) -> u16 {
        wrap_add(*self.size, idx, off)
    }

    /// Helper method to calculate a negative offset for a given index, wrapping
    /// at the size of the queue.
    fn idx_sub(&self, idx: u16, off: u16) -> u16 {
        wrap_sub(*self.size, idx, off)
    }
}
impl QueueGuard<'_, CompQueueState> {
    /// Attempt to return the Tail entry pointer and then move it forward by 1.
    ///
    /// If the queue is full this method returns [`None`].
    /// Otherwise, this method returns the current Tail entry pointer and then
    /// increments the Tail entry pointer by 1 (wrapping if necessary).
    fn push_tail(&mut self) -> Option<(u16, bool)> {
        if self.is_full() {
            return None;
        }
        let tail = self.state.tail;
        let phase = self.state.inner.phase;

        self.state.tail = self.idx_add(tail, 1);
        if self.state.tail < tail {
            // We wrapped, so flip phase
            self.state.inner.phase = !self.state.inner.phase;
        }
        Some((tail, phase))
    }

    /// How many slots are occupied between the head and the tail i.e., how
    /// many entries can we read from the queue currently.
    fn avail_occupied(&self) -> u16 {
        self.idx_sub(self.state.tail, self.state.head)
    }

    /// Attempt to move the Head entry pointer forward to the given index.
    ///
    /// The given index must be less than the size of the queue. The queue
    /// must have enough occupied slots otherwise we return an error.
    /// Conceptually this method indicates some entries have been consumed
    /// from the queue.
    fn pop_head_to(&mut self, idx: u16) -> Result<(), QueueUpdateError> {
        if u32::from(idx) >= *self.size {
            return Err(QueueUpdateError::InvalidEntry);
        }
        let pop_count = self.idx_sub(idx, self.state.head);
        if pop_count > self.avail_occupied() {
            return Err(QueueUpdateError::TooManyEntries);
        }
        // Replace head with given idx and update the number of available slots
        self.state.head = idx;
        self.state.inner.avail += pop_count;

        Ok(())
    }

    /// Is there available space in the CQ to push an entry?
    fn has_avail(&self) -> bool {
        self.state.inner.avail != 0
    }

    fn take_avail(&mut self, sq: &Arc<SubQueue>) -> bool {
        if let Some(avail) = self.state.inner.avail.checked_sub(1) {
            self.state.inner.avail = avail;
            true
        } else {
            // Make sure we kick the SQs when we have space available again
            self.record_corked(sq);
            false
        }
    }

    fn release_avail(&mut self) {
        if let Some(avail) = self.state.inner.avail.checked_add(1) {
            assert!(
                u32::from(avail) < *self.size,
                "attempted to overflow CQ available size"
            );
            self.state.inner.avail = avail;
        } else {
            panic!("attempted to overflow CQ available");
        }
    }

    /// Record an SQ as being corked on this CQ due to lack of permit capacity.
    fn record_corked(&mut self, sq: &Arc<SubQueue>) {
        self.state.inner.corked.insert(sq.id);
    }

    /// Get list of SQ IDs which were corked on this CQ
    fn kick(&mut self) -> Option<Vec<QueueId>> {
        if !self.state.inner.corked.is_empty() {
            Some(self.state.inner.corked.drain().collect())
        } else {
            None
        }
    }

    /// Write update to the EventIdx in Doorbell Buffer page, if possible
    fn db_buf_write(&mut self, devq_id: u64, mem: &MemCtx) {
        if let Some(db_buf) = self.state.db_buf {
            probes::nvme_cq_dbbuf_write!(|| (devq_id, self.state.tail));
            // Keep EventIdx populated with the position of the CQ tail.  We are
            // not especially concerned with receiving timely (doorbell) updates
            // from the guest about where the head pointer sits.  We keep our
            // own tally of how many entries are in the CQ are available for
            // completions to land.
            //
            // When checking for available space before issuing a Permit, we can
            // perform our own JIT read from the db_buf to stay updated on the
            // true space available.
            fence(Ordering::Release);
            mem.write(db_buf.eventidx, &self.state.tail);
        }
    }

    /// Write update to the Shadow Doorbell Buffer page, if possible.
    ///
    /// We would expect the guest driver to keep this value in a valid state per
    /// the specification, but qemu notes that certain consumers fail to do so
    /// on the admin queue.  We follow their lead to avoid issues.
    fn db_buf_write_shadow(&mut self, devq_id: u64, mem: &MemCtx) {
        if let Some(db_buf) = self.state.db_buf {
            probes::nvme_cq_dbbuf_write_shadow!(|| (devq_id, self.state.head));
            fence(Ordering::Release);
            mem.write(db_buf.shadow, &self.state.head);
        }
    }

    /// Read update from the Shadow in Doorbell Buffer page, if possible
    fn db_buf_read(&mut self, devq_id: u64, mem: &MemCtx) {
        if let Some(db_buf) = self.state.db_buf {
            if let Some(new_head) = mem.read::<u32>(db_buf.shadow) {
                let new_head = *new_head;
                probes::nvme_cq_dbbuf_read!(|| (
                    devq_id,
                    new_head,
                    self.state.head
                ));
                fence(Ordering::Acquire);
                // TODO: roll back on bad input?
                let _ = self.pop_head_to(new_head as u16);
            }
        }
    }
}

impl CompQueueState {
    /// Create a new `QueueState` for a Completion Queue
    fn new(size: u32, acc_mem: MemAccessor) -> QueueState<CompQueueState> {
        QueueState::new(
            size,
            acc_mem,
            CompQueueState {
                avail: (size - 1) as u16,
                // As the device side, we start with our phase tag as asserted (1)
                // since the host side (VM) will create all the Completion Queue
                // entries with the phase initially zeroed out.
                phase: true,
                corked: HashSet::new(),
            },
        )
    }
}

impl SubQueueState {
    /// Create a new `QueueState` for a Submission Queue
    fn new(size: u32, acc_mem: MemAccessor) -> QueueState<SubQueueState> {
        QueueState::new(
            size,
            acc_mem,
            SubQueueState { params: Default::default() },
        )
    }
}
impl QueueGuard<'_, SubQueueState> {
    /// How many slots are empty between the tail and the head i.e., how many
    /// entries can we write to the queue currently.
    fn avail_empty(&self) -> u16 {
        self.idx_sub(self.idx_sub(self.state.head, 1), self.state.tail)
    }

    /// Attempt to return the Head entry pointer and then move it forward by 1.
    ///
    /// If the queue is empty this method returns [`None`].
    /// Otherwise, this method returns the current Head entry pointer and then
    /// increments the Head entry pointer by 1 (wrapping if necessary).
    fn pop_head(&mut self, last_head: &AtomicU16) -> Option<u16> {
        if self.is_empty() {
            return None;
        } else {
            let old_head = self.state.head;
            self.state.head = self.idx_add(old_head, 1);
            last_head.store(self.state.head, Ordering::Release);
            Some(old_head)
        }
    }

    /// Attempt to move the Tail entry pointer forward to the given index.
    ///
    /// The given index must be less than the size of the queue. The queue must
    /// have enough empty slots available otherwise we return an error.
    /// Conceptually this method indicates new entries have been added to the
    /// queue.
    fn push_tail_to(&mut self, idx: u16) -> Result<(), QueueUpdateError> {
        if u32::from(idx) >= *self.size {
            return Err(QueueUpdateError::InvalidEntry);
        }
        let push_count = self.idx_sub(idx, self.state.tail);
        if push_count > self.avail_empty() {
            return Err(QueueUpdateError::TooManyEntries);
        }
        // Replace tail with given idx
        self.state.tail = idx;

        Ok(())
    }

    /// Write update to the EventIdx in Doorbell Buffer page, if possible
    fn db_buf_write(&mut self, devq_id: u64, mem: &MemCtx) {
        if let Some(db_buf) = self.state.db_buf {
            probes::nvme_sq_dbbuf_write!(|| (devq_id, self.state.head));
            // Keep EventIdx populated with the position of the SQ head.  As
            // long as there are entries available between the head and tail, we
            // do not want the guest taking exits for ultimately redundant
            // doorbells.
            //
            // We proactively read from the db_buf shadow while attempted to pop
            // entries submitted to the queue.  Only once it is empty, with the
            // head/tail being equal, do we want doorbell calls from the guest.
            fence(Ordering::Release);
            mem.write(db_buf.eventidx, &self.state.head);
        }
    }

    /// Write update to the Shadow Doorbell Buffer page, if possible.
    ///
    /// See [QueueGuard<SubQueueState>::db_buf_write_shadow()] for why we would
    /// write to a "guest-owned" page.
    fn db_buf_write_shadow(&mut self, devq_id: u64, mem: &MemCtx) {
        if let Some(db_buf) = self.state.db_buf {
            probes::nvme_sq_dbbuf_write_shadow!(|| (devq_id, self.state.tail));
            fence(Ordering::Release);
            mem.write(db_buf.shadow, &self.state.tail);
        }
    }

    /// Read update from the Shadow in Doorbell Buffer page, if possible
    fn db_buf_read(&mut self, devq_id: u64, mem: &MemCtx) {
        if let Some(db_buf) = self.state.db_buf {
            if let Some(new_tail) = mem.read::<u32>(db_buf.shadow) {
                let new_tail = *new_tail;
                probes::nvme_sq_dbbuf_read!(|| (
                    devq_id,
                    new_tail,
                    self.state.head
                ));
                fence(Ordering::Acquire);
                // TODO: roll back on bad input?
                let _ = self.push_tail_to(new_tail as u16);
            }
        }
    }
}

/// Errors that may be encountered during Queue creation.
#[derive(Error, Debug)]
pub enum QueueCreateErr {
    /// The specified base address is invalid.
    #[error("invalid base address")]
    InvalidBaseAddr,

    /// The specified length is invalid.
    #[error("invalid size")]
    InvalidSize,

    #[error("the SQ ID {0} is already associated with the CQ")]
    SubQueueIdAlreadyExists(QueueId),
}

/// Errors that may be encountered while adjusting Queue head/tail pointers.
#[derive(Error, Debug)]
pub enum QueueUpdateError {
    #[error("tried to move head or tail pointer to an invalid index")]
    InvalidEntry,

    #[error(
        "tried to push or pop too many entries given the current head/tail"
    )]
    TooManyEntries,
}

/// Basic parameters for Submission & Completion Queue creation
#[derive(Copy, Clone)]
pub struct CreateParams {
    pub id: QueueId,
    // Not strictly necessary for submission or completion queues, but helpful
    // to disambiguate the queue in probes.
    pub device_id: DeviceId,
    pub base: GuestAddr,
    pub size: u32,
}

/// Type for manipulating Submission Queues.
pub struct SubQueue {
    /// The ID of this Submission Queue.
    id: QueueId,

    /// The ID of the device that owns this submission queue. Kept here only to
    /// produce `devsq_id` for DTrace probes.
    device_id: DeviceId,

    /// The corresponding Completion Queue.
    cq: Arc<CompQueue>,

    /// Queue state such as the size and current head/tail entry pointers.
    state: QueueState<SubQueueState>,

    /// Duplicate of head pointer value from inside [SubQueueState], kept in
    /// sync for lockless access during [SubQueue::annotate_completion()] calls.
    cur_head: AtomicU16,

    /// The [`GuestAddr`] at which the Queue is mapped.
    base: GuestAddr,
}

impl Drop for SubQueue {
    fn drop(&mut self) {
        // Remove the CQ-SQ link
        let mut cq_sqs = self.cq.sqs.lock().unwrap();
        cq_sqs.remove(&self.id).unwrap();
    }
}

impl SubQueue {
    /// Create a Submission Queue object backed by the guest memory at the
    /// given base address.
    pub fn new(
        params: CreateParams,
        cq: Arc<CompQueue>,
        acc_mem: MemAccessor,
    ) -> Result<Arc<Self>, QueueCreateErr> {
        let CreateParams { id, device_id, base, size } = params;
        validate(id == ADMIN_QUEUE_ID, base, size)?;
        let sq = Arc::new(Self {
            id,
            device_id,
            cq,
            state: SubQueueState::new(size, acc_mem),
            cur_head: AtomicU16::new(0),
            base,
        });

        use std::collections::hash_map::Entry;
        // Associate this SQ with the given CQ
        let mut cq_sqs = sq.cq.sqs.lock().unwrap();
        match cq_sqs.entry(id) {
            Entry::Occupied(_) => {
                Err(QueueCreateErr::SubQueueIdAlreadyExists(id))
            }
            Entry::Vacant(entry) => {
                entry.insert(Arc::downgrade(&sq));
                drop(cq_sqs);
                Ok(sq)
            }
        }
    }

    /// Attempt to move the Tail entry pointer forward to the given index.
    pub fn notify_tail(&self, idx: u16) -> Result<u16, QueueUpdateError> {
        let mut state = self.state.lock();
        state.push_tail_to(idx)?;
        if self.id == ADMIN_QUEUE_ID {
            if let Some(mem) = state.acc_mem.access() {
                state.db_buf_write_shadow(self.devq_id(), &mem);
            }
        }

        Ok(state.num_occupied())
    }

    pub fn num_occupied(&self) -> u16 {
        self.state.lock().num_occupied()
    }

    /// Returns the next entry off of the Queue or [`None`] if it is empty.
    pub fn pop(
        self: &Arc<SubQueue>,
    ) -> Option<(GuestData<SubmissionQueueEntry>, Permit, u16)> {
        let Some(mem) = self.state.acc_mem.access() else { return None };

        // Attempt to reserve an entry on the Completion Queue
        let permit = self.cq.reserve_entry(&self, &mem)?;
        let mut state = self.state.lock();

        // Check for last-minute updates to the tail via any configured doorbell
        // page, prior to attempting the pop itself.
        state.db_buf_read(self.devq_id(), &mem);

        if let Some(idx) = state.pop_head(&self.cur_head) {
            let addr = self.base.offset::<SubmissionQueueEntry>(idx as usize);

            if let Some(ent) = mem.read::<SubmissionQueueEntry>(addr) {
                let devq_id = self.devq_id();
                state.db_buf_write(devq_id, &mem);
                state.db_buf_read(devq_id, &mem);
                return Some((ent, permit.promote(ent.cid()), idx));
            }
            // TODO: set error state on queue/ctrl if we cannot read entry
        }

        // Drop lock on SQ before releasing permit (which locks CQ)
        drop(state);

        // No Submission Queue entry, so return the CQE permit
        permit.remit();
        None
    }

    /// Returns the ID of this Submission Queue.
    pub(super) fn id(&self) -> QueueId {
        self.id
    }

    pub(super) fn set_db_buf(
        &self,
        db_buf: Option<DoorbellBuffer>,
        is_import: bool,
    ) {
        let mut state = self.state.lock();
        state.state.db_buf =
            db_buf.map(|dbb| dbb.offset_for_queue(false, self.id()));

        if !is_import {
            // Mimic qemu and sync out the SQ tail during setup
            if let (Some(mem), Some(db_buf)) =
                (state.acc_mem.access(), state.state.db_buf)
            {
                mem.write(db_buf.shadow, &(state.state.tail as u32));
            }
        }
    }

    pub(super) fn update_params(&self, params: TransferParams) {
        self.state.lock().state.inner.params = params;
    }
    pub(super) fn params(&self) -> TransferParams {
        self.state.lock().state.inner.params
    }

    /// Annotate a CQE with data (ID and head index) from this SQ
    fn annotate_completion(&self, cqe: &mut CompletionQueueEntry) {
        cqe.sqid = self.id;
        cqe.sqhd = self.cur_head.load(Ordering::Acquire);
    }

    /// Return a VM-unique identifier for this submission queue
    pub(crate) fn devq_id(&self) -> u64 {
        super::devq_id(self.device_id, self.id)
    }

    pub(super) fn export(&self) -> migrate::NvmeSubQueueV1 {
        let inner = self.state.inner.lock().unwrap();
        migrate::NvmeSubQueueV1 {
            id: self.id,
            size: self.state.size,
            head: inner.head,
            tail: inner.tail,
            base: self.base.0,
            cq_id: self.cq.id,
        }
    }

    pub(super) fn import(
        &self,
        state: migrate::NvmeSubQueueV1,
    ) -> Result<(), MigrateStateError> {
        // These must've been provided at construction
        assert_eq!(self.id, state.id);
        assert_eq!(self.cq.id, state.cq_id);
        assert_eq!(self.base.0, state.base);
        assert_eq!(self.state.size, state.size);

        let mut inner = self.state.inner.lock().unwrap();
        inner.head = state.head;
        inner.tail = state.tail;

        Ok(())
    }
}

/// Type for manipulating Completion Queues.
pub struct CompQueue {
    /// The ID of this Completion Queue.
    id: QueueId,

    /// The ID of the device that owns this completion queue. Kept here only to
    /// produce `devcq_id` for DTrace probes.
    device_id: DeviceId,

    /// The Interrupt Vector used to signal to the host (VM) upon pushing
    /// entries onto the Completion Queue.
    iv: u16,

    /// Queue state such as the size and current head/tail entry pointers.
    state: QueueState<CompQueueState>,

    /// The [`GuestAddr`] at which the Queue is mapped.
    base: GuestAddr,

    /// MSI-X object associated with PCIe device to signal host (VM).
    hdl: pci::MsixHdl,

    /// [`SubQueue`]'s associated with this Completion Queue.
    sqs: Mutex<HashMap<QueueId, Weak<SubQueue>>>,
}

impl CompQueue {
    /// Creates a Completion Queue object backed by the guest memory at the
    /// given base address.
    pub fn new(
        params: CreateParams,
        iv: u16,
        hdl: pci::MsixHdl,
        acc_mem: MemAccessor,
    ) -> Result<Self, QueueCreateErr> {
        let CreateParams { id, device_id, base, size } = params;
        validate(id == ADMIN_QUEUE_ID, base, size)?;
        Ok(Self {
            id,
            device_id,
            iv,
            state: CompQueueState::new(size, acc_mem),
            base,
            hdl,
            sqs: Mutex::new(HashMap::new()),
        })
    }

    /// Attempt to move the Head entry pointer forward to the given index.
    pub fn notify_head(&self, idx: u16) -> Result<(), QueueUpdateError> {
        let mut state = self.state.lock();
        state.pop_head_to(idx)?;
        if self.id == ADMIN_QUEUE_ID {
            if let Some(mem) = state.acc_mem.access() {
                state.db_buf_write_shadow(self.devq_id(), &mem)
            }
        }
        Ok(())
    }

    /// Fires an interrupt to the guest with the associated interrupt vector
    /// if the queue is not currently empty.
    pub fn fire_interrupt(&self) {
        let state = self.state.lock();
        if !state.is_empty() {
            self.hdl.fire(self.iv);
        }
    }

    /// Returns whether the SQIDs should be kicked due to no permits being
    /// available previously.
    ///
    /// If the value was true, it will also get reset to false.
    pub fn kick(&self) -> Option<Vec<QueueId>> {
        self.state.lock().kick()
    }

    /// Returns the ID of this Completion Queue.
    pub fn id(&self) -> QueueId {
        self.id
    }

    /// Returns the number of SQs associated with this Completion Queue.
    pub fn associated_sqs(&self) -> usize {
        let sqs = self.sqs.lock().unwrap();
        sqs.len()
    }

    /// Attempt to reserve an entry in the Completion Queue.
    ///
    /// An entry permit allows the user to push onto the Completion Queue.
    fn reserve_entry(
        self: &Arc<Self>,
        sq: &Arc<SubQueue>,
        mem: &MemCtx,
    ) -> Option<ProtoPermit> {
        let mut state = self.state.lock();
        if !state.has_avail() {
            // If the CQ appears full, but the db_buf shadow is configured, do a
            // last-minute check to see if entries have been consumed/freed
            // without a doorbell call.
            state.db_buf_read(self.devq_id(), mem);
        }
        if state.take_avail(sq) {
            Some(ProtoPermit::new(self, sq))
        } else {
            // No more spots available.
            None
        }
    }

    /// Add a new entry to the Completion Queue while consuming a `Permit`.
    fn push(&self, comp: Completion, cid: u16, sq: &SubQueue) {
        let mut cqe = CompletionQueueEntry::new(comp, cid);
        sq.annotate_completion(&mut cqe);

        let mut state = self.state.lock();
        let (idx, phase) = state
            .push_tail()
            .expect("CQ should have available space for assigned permit");

        probes::nvme_cqe!(|| (self.devq_id(), idx, u8::from(phase)));

        // The only definite indicator that a CQE has become valid is the phase
        // bit being toggled.  Since the interface for writing to guest memory
        // cannot ensure that the other bits of the CQE are written before the
        // phase bit, we must proceed carefully:
        //
        // The CQE is first written with the opposite phase set, so it appears
        // to the guest OS as a to-be-filled entry.  With that write complete,
        // ensuring that all fields of the CQE are visible to the host, we write
        // it again, with the phase bit correctly set.
        //
        // XXX: handle a guest addr that becomes unmapped later
        let addr = self.base.offset::<CompletionQueueEntry>(idx as usize);
        if let Some(mem) = self.state.acc_mem.access() {
            cqe.set_phase(!phase);
            mem.write(addr, &cqe);
            cqe.set_phase(phase);
            mem.write(addr, &cqe);

            let devq_id = self.devq_id();
            state.db_buf_read(devq_id, &mem);
            state.db_buf_write(devq_id, &mem);
        } else {
            // TODO: mark the queue/controller in error state?
        }
    }

    pub(super) fn set_db_buf(
        &self,
        db_buf: Option<DoorbellBuffer>,
        is_import: bool,
    ) {
        let mut state = self.state.lock();
        state.state.db_buf =
            db_buf.map(|dbb| dbb.offset_for_queue(true, self.id()));

        if !is_import {
            // Mimic qemu and sync out the CQ head during setup
            if let (Some(mem), Some(db_buf)) =
                (state.acc_mem.access(), state.state.db_buf)
            {
                mem.write(db_buf.shadow, &(state.state.head as u32));
            }
        }
    }

    /// Return a VM-unique identifier for this completion queue
    pub(crate) fn devq_id(&self) -> u64 {
        super::devq_id(self.device_id, self.id)
    }

    pub(super) fn export(&self) -> migrate::NvmeCompQueueV1 {
        let guard = self.state.lock();
        migrate::NvmeCompQueueV1 {
            id: self.id,
            size: self.state.size,
            head: guard.state.head,
            tail: guard.state.tail,
            avail: guard.state.inner.avail,
            phase: guard.state.inner.phase,
            base: self.base.0,
            iv: self.iv,
        }
    }

    pub(super) fn import(
        &self,
        state: migrate::NvmeCompQueueV1,
    ) -> Result<(), MigrateStateError> {
        // These must've been provided at construction
        assert_eq!(self.id, state.id);
        assert_eq!(self.iv, state.iv);
        assert_eq!(self.base.0, state.base);
        assert_eq!(self.state.size, state.size);

        let mut guard = self.state.lock();
        guard.state.head = state.head;
        guard.state.tail = state.tail;
        guard.state.inner.avail = state.avail;
        guard.state.inner.phase = state.phase;

        Ok(())
    }
}

/// "Proto" permit for a Completion Queue Entry.
///
/// This guarantees the holder capacity in the associated Completion Queue to
/// push their CQE.  A `ProtoPermit` is either promoted to a full [Permit] via
/// [promote()](Self::promote), or if no submissions were found to be available
/// after reserving the `ProtoPermit`, discarded to release its reservation via
/// [remit()](Self::remit).
pub struct ProtoPermit {
    /// The corresponding Completion Queue for which we have a permit.
    cq: Weak<CompQueue>,

    /// The Submission Queue for which this entry is reserved.
    sq: Weak<SubQueue>,

    /// The ID for the device and Submission Queue the command associated with
    /// this permit was submtited from. Stored separately to avoid going through
    /// the Weak ref.
    devsq_id: u64,
}
impl ProtoPermit {
    fn new(cq: &Arc<CompQueue>, sq: &Arc<SubQueue>) -> Self {
        Self {
            cq: Arc::downgrade(cq),
            sq: Arc::downgrade(sq),
            devsq_id: sq.devq_id(),
        }
    }

    /// Promote a "proto" permit to a [Permit].
    ///
    /// Once an entry has been read from the Submission Queue, the holder of a
    /// `ProtoPermit` promotes it to `Permit`, committing to use the reserved
    /// CQE capacity when the submission is processed.
    pub fn promote(self, cid: u16) -> Permit {
        Permit {
            cq: self.cq,
            sq: self.sq,
            devsq_id: self.devsq_id,
            cid,
            _nodrop: NoDropPermit,
        }
    }

    /// Return the permit without having actually used it.
    ///
    /// Frees up the space for someone else to grab it via
    /// `CompQueue::reserve_entry`.
    fn remit(self) {
        if let Some(cq) = self.cq.upgrade() {
            let mut state = cq.state.lock();
            state.release_avail();
        }
    }
}

/// A permit reserving capacity to push a [CompletionQueueEntry] into a
/// Completion Queue for a command submitted to the device.
pub struct Permit {
    /// The corresponding Completion Queue for which we have a permit.
    cq: Weak<CompQueue>,

    /// The Submission Queue for which this entry is reserved.
    sq: Weak<SubQueue>,

    /// The Submission Queue and device ID the request came in on. Retained as a
    /// consistent source identifier for probes.
    devsq_id: u64,

    /// ID of command holding this permit.  Used to populate `cid` field in
    /// Completion Queue Entry.
    cid: u16,

    /// Marker to ensure holder calls [Permit::complete()].
    _nodrop: NoDropPermit,
}

impl Permit {
    /// Consume the permit by placing an entry into the Completion Queue.
    pub fn complete(self, comp: Completion) {
        let Permit { cq, sq, cid, _nodrop, .. } = self;
        std::mem::forget(_nodrop);

        let cq = match cq.upgrade() {
            Some(cq) => cq,
            None => {
                // The CQ has since been deleted so no way to complete this
                // request nor to return the permit.
                debug_assert!(sq.upgrade().is_none());
                return;
            }
        };

        if let Some(sq) = sq.upgrade() {
            cq.push(comp, cid, &sq);
            cq.fire_interrupt();
        } else {
            // The SQ has since been deleted (so the request has already
            // implicitly been aborted by the prior Delete Queue command) or
            // the device currently lacks access to guest memory.
            //
            // Just make sure we return the "avail hold" from the permit
            let mut state = cq.state.lock();
            state.release_avail();
        }
    }

    /// Get the ID of the submitted command associated with this permit.
    pub fn cid(&self) -> u16 {
        self.cid
    }

    /// Get the ID of the device and Submission Queue the command associated
    /// with this permit was submitted on.
    pub fn devsq_id(&self) -> u64 {
        self.devsq_id
    }

    /// A device reset may cause us to abandon some in-flight I/O, dropping the
    /// request `Permit` without any kind of completion.  Additionally, some of
    /// the tests which to acquire Permit entries with no intent to drive them
    /// through to completion.  Allow them to bypass the
    /// ensure-this-permit-is-completed check in [`Drop`].
    pub fn abandon(self) {
        let Permit { _nodrop, .. } = self;
        std::mem::forget(_nodrop);
    }

    /// Consume the permit by placing an entry into the Completion Queue.
    ///
    /// This is a simpler version of [Self::complete()] for testing purposes
    /// which does not require passing in the actual completion data.  It is
    /// only to be used for exercising the Submission and Completion Queues in
    /// unit tests.
    #[cfg(test)]
    fn test_complete(self, sq: &SubQueue) {
        let Permit { cq, cid, _nodrop, .. } = self;
        std::mem::forget(_nodrop);

        if let Some(cq) = cq.upgrade() {
            cq.push(Completion::success(), cid, sq);
        }
    }
}
impl Debug for Permit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Permit")
            .field("sq", &self.sq)
            .field("cq", &self.cq)
            .field("devsq_id", &self.devsq_id)
            .field("cid", &self.cid)
            .finish()
    }
}

/// Marker struct to ensure that [Permit] consumers call
/// [complete()](Permit::complete()), rather than silently dropping it.
struct NoDropPermit;
impl Drop for NoDropPermit {
    fn drop(&mut self) {
        panic!("Permit should be complete()-ed before drop");
    }
}

/// Convert IO SQID to block-layer [block::QueueId]
pub fn sqid_to_block_qid(sqid: super::QueueId) -> block::QueueId {
    // With the admin SQ occupying ID 0, the corresponding block-layer queue IDs
    // are offset by 1
    sqid.checked_sub(1).expect("IO SQID is non-zero").into()
}

#[derive(Copy, Clone, Debug, Default)]
pub struct TransferParams {
    pub lba_data_size: u64,
    pub max_data_tranfser_size: u64,
}

/// Configuration for Doorbell Buffer feature
#[derive(Copy, Clone, Debug)]
pub struct DoorbellBuffer {
    pub shadow: GuestAddr,
    pub eventidx: GuestAddr,
}
impl DoorbellBuffer {
    /// Determine Shadow Doorbell and EventIdx addresses for a specified
    /// [QueueId] and stride configuration.
    pub fn offset_for_queue(&self, is_cq: bool, qid: QueueId) -> Self {
        let idx: usize = (qid as usize) * 2 + is_cq.then_some(1).unwrap_or(0);
        Self {
            shadow: self.shadow.offset::<u32>(idx),
            eventidx: self.eventidx.offset::<u32>(idx),
        }
    }
}

pub(super) mod migrate {
    use serde::{Deserialize, Serialize};

    #[derive(Deserialize, Serialize)]
    pub struct NvmeCompQueueV1 {
        pub id: u16,

        pub size: u32,
        pub head: u16,
        pub tail: u16,

        pub avail: u16,
        pub phase: bool,

        pub base: u64,
        pub iv: u16,
    }

    #[derive(Deserialize, Serialize)]
    pub struct NvmeSubQueueV1 {
        pub id: u16,

        pub size: u32,
        pub head: u16,
        pub tail: u16,

        pub base: u64,
        pub cq_id: u16,
    }
}

#[cfg(test)]
mod test {
    use rand::Rng;

    use super::*;

    use crate::{common::GuestAddr, vmm::Machine};
    use std::io::Error;
    use std::thread::{sleep, spawn};
    use std::time::Duration;

    #[test]
    fn create_cqs() -> Result<(), Error> {
        let machine = Machine::new_test()?;
        let hdl = pci::MsixHdl::new_test();
        let write_base = GuestAddr(1024 * 1024);
        let tmpl = CreateParams {
            id: ADMIN_QUEUE_ID,
            device_id: DeviceId(0),
            base: write_base,
            size: 0,
        };

        let acc_mem = || machine.acc_mem.child(None);

        // Admin queues must be less than 4K
        let cq = CompQueue::new(
            CreateParams {
                id: ADMIN_QUEUE_ID,
                device_id: DeviceId(0),
                size: 1024,
                ..tmpl
            },
            0,
            hdl.clone(),
            acc_mem(),
        );
        assert!(matches!(cq, Ok(_)));
        let cq = CompQueue::new(
            CreateParams {
                id: ADMIN_QUEUE_ID,
                device_id: DeviceId(0),
                size: 5 * 1024,
                ..tmpl
            },
            0,
            hdl.clone(),
            acc_mem(),
        );
        assert!(matches!(cq, Err(QueueCreateErr::InvalidSize)));

        // I/O queues must be less than 64K
        let cq = CompQueue::new(
            CreateParams { id: 1, device_id: DeviceId(0), size: 1024, ..tmpl },
            0,
            hdl.clone(),
            acc_mem(),
        );
        assert!(matches!(cq, Ok(_)));
        let cq = CompQueue::new(
            CreateParams {
                id: 1,
                device_id: DeviceId(0),
                size: 65 * 1024,
                ..tmpl
            },
            0,
            hdl.clone(),
            acc_mem(),
        );
        assert!(matches!(cq, Err(QueueCreateErr::InvalidSize)));

        // Neither must be less than 2
        let cq = CompQueue::new(
            CreateParams {
                id: ADMIN_QUEUE_ID,
                device_id: DeviceId(0),
                size: 1,
                ..tmpl
            },
            0,
            hdl.clone(),
            acc_mem(),
        );
        assert!(matches!(cq, Err(QueueCreateErr::InvalidSize)));
        let cq = CompQueue::new(
            CreateParams { id: 1, device_id: DeviceId(0), size: 1, ..tmpl },
            0,
            hdl.clone(),
            acc_mem(),
        );
        assert!(matches!(cq, Err(QueueCreateErr::InvalidSize)));

        Ok(())
    }

    #[test]
    fn create_sqs() -> Result<(), Error> {
        let machine = Machine::new_test()?;
        let hdl = pci::MsixHdl::new_test();
        let read_base = GuestAddr(0);
        let write_base = GuestAddr(1024 * 1024);

        let acc_mem = || machine.acc_mem.child(None);

        // Create corresponding CQs
        let admin_cq = Arc::new(
            CompQueue::new(
                CreateParams {
                    id: ADMIN_QUEUE_ID,
                    device_id: DeviceId(0),
                    base: write_base,
                    size: 1024,
                },
                0,
                hdl.clone(),
                acc_mem(),
            )
            .unwrap(),
        );
        let io_cq = Arc::new(
            CompQueue::new(
                CreateParams {
                    id: 1,
                    device_id: DeviceId(0),
                    base: write_base,
                    size: 1024,
                },
                0,
                hdl,
                acc_mem(),
            )
            .unwrap(),
        );

        // Admin queues must be less than 4K
        let sq = SubQueue::new(
            CreateParams {
                id: ADMIN_QUEUE_ID,
                device_id: DeviceId(0),
                base: read_base,
                size: 1024,
            },
            admin_cq.clone(),
            acc_mem(),
        );
        assert!(matches!(sq, Ok(_)));
        let sq = SubQueue::new(
            CreateParams {
                id: ADMIN_QUEUE_ID,
                device_id: DeviceId(0),
                base: read_base,
                size: 5 * 1024,
            },
            admin_cq.clone(),
            acc_mem(),
        );
        assert!(matches!(sq, Err(QueueCreateErr::InvalidSize)));

        // I/O queues must be less than 64K
        let sq = SubQueue::new(
            CreateParams {
                id: 1,
                device_id: DeviceId(0),
                base: read_base,
                size: 1024,
            },
            io_cq.clone(),
            acc_mem(),
        );
        assert!(matches!(sq, Ok(_)));
        let sq = SubQueue::new(
            CreateParams {
                id: 1,
                device_id: DeviceId(0),
                base: read_base,
                size: 65 * 1024,
            },
            io_cq,
            acc_mem(),
        );
        assert!(matches!(sq, Err(QueueCreateErr::InvalidSize)));

        // Neither must be less than 2
        let sq = SubQueue::new(
            CreateParams {
                id: ADMIN_QUEUE_ID,
                device_id: DeviceId(0),
                base: read_base,
                size: 1,
            },
            admin_cq.clone(),
            acc_mem(),
        );
        assert!(matches!(sq, Err(QueueCreateErr::InvalidSize)));
        let sq = SubQueue::new(
            CreateParams {
                id: 1,
                device_id: DeviceId(0),
                base: read_base,
                size: 1,
            },
            admin_cq,
            acc_mem(),
        );
        assert!(matches!(sq, Err(QueueCreateErr::InvalidSize)));

        // Completion Queue's must be mapped to readable memory
        //
        // This relied on the test machinery establishing the writable memory
        // region as write-only.  Until we expose such a region, it is not clear
        // how much value such a test brings to the table.
        //
        // let sq = SubQueue::new(
        //     ADMIN_QUEUE_ID,
        //     admin_cq.clone(),
        //     2,
        //     write_base,
        //     &mem,
        // );
        // assert!(matches!(sq, Err(QueueCreateErr::InvalidBaseAddr)));
        // let sq = SubQueue::new(1, admin_cq.clone(), 2, write_base, &mem);
        // assert!(matches!(sq, Err(QueueCreateErr::InvalidBaseAddr)));

        Ok(())
    }

    #[test]
    fn push_failures() -> Result<(), Error> {
        let machine = Machine::new_test()?;
        let hdl = pci::MsixHdl::new_test();
        let read_base = GuestAddr(0);
        let write_base = GuestAddr(1024 * 1024);

        let acc_mem = || machine.acc_mem.child(None);

        // Create our queues
        let cq = Arc::new(
            CompQueue::new(
                CreateParams {
                    id: 1,
                    device_id: DeviceId(0),
                    base: write_base,
                    size: 4,
                },
                0,
                hdl,
                acc_mem(),
            )
            .unwrap(),
        );
        let sq = Arc::new(
            SubQueue::new(
                CreateParams {
                    id: 1,
                    device_id: DeviceId(0),
                    base: read_base,
                    size: 4,
                },
                cq.clone(),
                acc_mem(),
            )
            .unwrap(),
        );

        // Replicate guest VM notifying us things were pushed to the SQ
        let mut sq_tail = 0;
        for _ in 0..sq.state.size - 1 {
            sq_tail = wrap_add(sq.state.size, sq_tail, 1);
            // These should all succeed
            assert!(matches!(sq.notify_tail(sq_tail), Ok(_)));
        }

        // But anything more should fail
        sq_tail = wrap_add(sq.state.size, sq_tail, 1);
        assert!(matches!(
            sq.notify_tail(sq_tail),
            Err(QueueUpdateError::TooManyEntries)
        ));

        // Also anything that falls outside the boundaries (i.e. we didn't wrap properly)
        assert!(matches!(
            sq.notify_tail(sq.state.size as u16),
            Err(QueueUpdateError::InvalidEntry)
        ));

        // Now pop those SQ items and complete them in the CQ
        while let Some((_, permit, _)) = sq.pop() {
            permit.test_complete(&sq);
        }

        // Replicate guest VM notifying us things were consumed off the CQ
        let mut cq_head = 0;
        for _ in 0..sq.state.size - 1 {
            cq_head = wrap_add(cq.state.size, cq_head, 1);
            // These should all succeed
            assert!(matches!(cq.notify_head(cq_head), Ok(_)));
        }

        // There's nothing else to pop so this should fail
        cq_head = wrap_add(cq.state.size, cq_head, 1);
        assert!(matches!(
            cq.notify_head(cq_head),
            Err(QueueUpdateError::TooManyEntries)
        ));

        // Also anything that falls outside the boundaries (i.e. we didn't wrap properly)
        assert!(matches!(
            cq.notify_head(cq.state.size as u16),
            Err(QueueUpdateError::InvalidEntry)
        ));

        Ok(())
    }

    #[test]
    fn cq_kicks() -> Result<(), Error> {
        let machine = Machine::new_test()?;
        let hdl = pci::MsixHdl::new_test();
        let read_base = GuestAddr(0);
        let write_base = GuestAddr(1024 * 1024);

        let acc_mem = || machine.acc_mem.child(None);

        // Create our queues
        // Purposely make the CQ smaller to test kicks
        let cq = Arc::new(
            CompQueue::new(
                CreateParams {
                    id: 1,
                    device_id: DeviceId(0),
                    base: write_base,
                    size: 2,
                },
                0,
                hdl,
                acc_mem(),
            )
            .unwrap(),
        );
        let sq = Arc::new(
            SubQueue::new(
                CreateParams {
                    id: 1,
                    device_id: DeviceId(0),
                    base: read_base,
                    size: 4,
                },
                cq.clone(),
                acc_mem(),
            )
            .unwrap(),
        );

        // Replicate guest VM notifying us things were pushed to the SQ
        let mut sq_tail = 0;
        for _ in 0..sq.state.size - 1 {
            sq_tail = wrap_add(sq.state.size, sq_tail, 1);
            assert!(matches!(sq.notify_tail(sq_tail), Ok(_)));
        }

        // We should be able to pop based on how much space is in the CQ
        for _ in 0..cq.state.size - 1 {
            let pop = sq.pop();
            assert!(matches!(pop, Some(_)));

            // Complete these in the CQ (but note guest won't have acknowledged them yet)
            pop.unwrap().1.test_complete(&sq);
        }

        // But we can't pop anymore due to no more CQ space to reserve
        assert!(matches!(sq.pop(), None));

        // The guest consuming things off the CQ should let free us
        assert!(matches!(cq.notify_head(1), Ok(_)));

        // Kick should've been set in the failed pop
        assert!(cq.kick().is_some());

        // We should have one more space now and should be able to pop 1 more
        assert!(matches!(
            sq.pop().map(|(_sub, permit, _idx)| {
                // ignore permit so it can be discarded when done
                permit.abandon()
            }),
            Some(_)
        ));

        Ok(())
    }

    #[test]
    fn push_pop() -> Result<(), Error> {
        let machine = Machine::new_test()?;
        let hdl = pci::MsixHdl::new_test();
        let read_base = GuestAddr(0);
        let write_base = GuestAddr(1024 * 1024);

        let acc_mem = || machine.acc_mem.child(None);

        // Create a pair of Completion and Submission Queues
        // with a random size. We purposefully give the CQ a smaller
        // size to exercise the "kick" conditions where we have some
        // request available in the SQ but can't pop it until there's
        // space available in the CQ.
        let mut rng = rand::rng();
        let sq_size = rng.random_range(512..2048);
        let cq = Arc::new(
            CompQueue::new(
                CreateParams {
                    id: 1,
                    device_id: DeviceId(0),
                    base: write_base,
                    size: 4,
                },
                0,
                hdl,
                acc_mem(),
            )
            .unwrap(),
        );
        let sq = Arc::new(
            SubQueue::new(
                CreateParams {
                    id: 1,
                    device_id: DeviceId(0),
                    base: read_base,
                    size: sq_size,
                },
                cq.clone(),
                acc_mem(),
            )
            .unwrap(),
        );

        // We'll be generating a random number of submissions
        let submissions_rand = rng.random_range(2..sq.state.size - 1);

        let (doorbell_tx, doorbell_rx) =
            crossbeam_channel::unbounded::<Doorbell>();
        let (workers_tx, workers_rx) = crossbeam_channel::unbounded();
        let (comp_tx, comp_rx) = crossbeam_channel::unbounded();

        // Create a thread to mimic the main device thread that
        // will handle "doorbell" read/write ops.
        enum Doorbell {
            Cq(u16),
            Sq(u16),
        }
        let (doorbell_cq, doorbell_sq) = (cq, sq.clone());
        let doorbell_handler = spawn(move || {
            // Keep track of the "host" side CQ head and SQ tail as
            // we receive "doorbell" hits.
            let mut cq_head = 0;
            let mut sq_tail = 0;
            loop {
                match doorbell_rx.recv() {
                    Ok(Doorbell::Cq(n)) => {
                        cq_head = wrap_add(doorbell_cq.state.size, cq_head, n);
                        assert!(matches!(
                            doorbell_cq.notify_head(cq_head),
                            Ok(_)
                        ));
                        if doorbell_cq.kick().is_some() {
                            assert!(workers_tx.send(()).is_ok());
                        }
                    }
                    Ok(Doorbell::Sq(n)) => {
                        sq_tail = wrap_add(doorbell_sq.state.size, sq_tail, n);
                        // The "doorbell" was rung and so let's have the SQ
                        // update its internal state before poking the workers
                        assert!(matches!(
                            doorbell_sq.notify_tail(sq_tail),
                            Ok(_)
                        ));
                        assert!(workers_tx.send(()).is_ok());
                    }
                    Err(_) => break,
                }
            }
        });

        // Create a number of worker threads to simulate the block
        // dev backend workers that will be notified every time the
        // SQ "doorbell" is hit and will attempt to pull a new IO
        // request off the SQ. At the end, each will return a count
        // of how many requests they received and then completed.
        //
        // N.B. Clippy is too aggressive here; the `collect` is needed to
        //      ensure the closure is evaluated, which is what actually
        //      launches the worker threads.
        #[allow(clippy::needless_collect)]
        let io_workers = (0..4)
            .map(|_| {
                let worker_rx = workers_rx.clone();
                let worker_sq = sq.clone();
                let worker_comp_tx = comp_tx.clone();

                spawn(move || {
                    let mut submissions = 0;

                    let mut rng = rand::rng();
                    while let Ok(()) = worker_rx.recv() {
                        while let Some((_, cqe_permit, _)) = worker_sq.pop() {
                            submissions += 1;

                            // Sleep for a bit to mimic actually doing
                            // some work before we complete the IO
                            sleep(Duration::from_micros(
                                rng.random_range(0..500),
                            ));

                            cqe_permit.test_complete(&worker_sq);

                            // Signal "guest" side of completion handler
                            assert!(worker_comp_tx.send(()).is_ok());
                        }
                    }
                    submissions
                })
            })
            .collect::<Vec<_>>();

        // Create a thread to "consume" things off the Completion Queue. This
        // simulates the host reacting to our CQ pushes and "ringing" the CQ
        // doorbell. At the end, it returns how many completion were handled.
        // Regardless, it'll stop after the number of completions is at least as
        // many as the number of submissions we decided to generate.
        let comp_doorbell_tx = doorbell_tx.clone();
        let comp_handler = spawn(move || {
            let exit_after = submissions_rand;
            let mut completions = 0;
            loop {
                match comp_rx.recv() {
                    Ok(()) => {
                        // "Ring" the CQ doorbell
                        // TODO: test completing more than 1 at a time
                        assert!(comp_doorbell_tx.send(Doorbell::Cq(1)).is_ok());
                        completions += 1;
                    }
                    Err(_) => break completions,
                }
                if completions >= exit_after {
                    break completions;
                }
            }
        });

        // Now, start generating a random number of submissions
        for _ in 0..submissions_rand {
            // "Ring" the SQ doorbell
            // TODO: test submitting more than 1 at a time
            //let doorbell_tx = doorbell_tx.clone();
            assert!(doorbell_tx.send(Doorbell::Sq(1)).is_ok());

            // Sleep up to 100us in between
            sleep(Duration::from_micros(rng.random_range(0..100)));
        }
        drop(doorbell_tx);

        // Wait for the completion handler and its count
        let completions: u32 = comp_handler.join().unwrap();

        // Wait for doorbell handler
        doorbell_handler.join().unwrap();

        // Wait for the IO workers to complete and sum the total
        // number of submissions they recevied
        let submissions: u32 =
            io_workers.into_iter().map(|j| j.join().unwrap()).sum();

        // Make sure the number of submission we recevied matched the number we
        // generated and completed
        assert_eq!(submissions, submissions_rand);
        assert_eq!(submissions, completions);

        Ok(())
    }
}
