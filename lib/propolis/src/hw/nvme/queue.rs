// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::collections::HashMap;
use std::fmt::Debug;
use std::mem::size_of;
use std::sync::{Arc, Mutex, MutexGuard, Weak};

use super::bits::{CompletionQueueEntry, SubmissionQueueEntry};
use super::cmds::Completion;
use crate::common::*;
use crate::hw::pci;
use crate::migrate::MigrateStateError;
use crate::vmm::MemCtx;

use thiserror::Error;

#[usdt::provider(provider = "propolis")]
mod probes {
    fn nvme_cqe(qid: u16, idx: u16, phase: u8) {}
}

/// Each queue is identified by a 16-bit ID.
///
/// See NVMe 1.0e Section 4.1.4 Queue Identifier
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
#[derive(Debug)]
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

    /// Whether the CQ should kick its SQs due to no permits being available
    /// previously.
    ///
    /// One may only pop something off the SQ if there's at least one space
    /// available in the corresponding CQ. If there isn't, we set the kick flag.
    kick: bool,
}

/// Submission Queue State
#[derive(Debug)]
struct SubQueueState();

/// Helper for manipulating Completion/Submission Queues
///
/// The type parameter `QS` is used to constrain the set of methods exposed
/// based on whether the queue in question is a Completion or Submission queue.
///
/// Use either [CompQueueState] or [SubQueueState].
#[derive(Debug)]
struct QueueState<QS: Debug> {
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
}
impl<QS: Debug> QueueState<QS> {
    fn new(size: u32, inner: QS) -> Self {
        assert!(size >= MIN_QUEUE_SIZE && size <= MAX_QUEUE_SIZE);
        Self { size, inner: Mutex::new(QueueInner { head: 0, tail: 0, inner }) }
    }
    fn lock(&self) -> QueueGuard<QS> {
        QueueGuard { size: &self.size, state: self.inner.lock().unwrap() }
    }
}

fn wrap_add(size: u32, idx: u16, off: u16) -> u16 {
    debug_assert!((idx as u32) < size);
    debug_assert!((off as u32) < size);

    let res = idx as u32 + off as u32;
    if res >= size {
        (res - size) as u16
    } else {
        res as u16
    }
}
fn wrap_sub(size: u32, idx: u16, off: u16) -> u16 {
    debug_assert!((idx as u32) < size);
    debug_assert!((off as u32) < size);

    if off > idx {
        ((idx as u32 + size) - off as u32) as u16
    } else {
        idx - off
    }
}

#[derive(Debug)]
struct QueueInner<QS: Debug> {
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

    /// Additional state specific to the queue type (completion or submission)
    inner: QS,
}

struct QueueGuard<'a, QS: Debug> {
    state: MutexGuard<'a, QueueInner<QS>>,
    size: &'a u32,
}
impl<'a, QS: Debug> QueueGuard<'a, QS> {
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

    fn head(&self) -> u16 {
        self.state.head
    }

    /// Helper method to calculate a positive offset for a given index, wrapping at
    /// the size of the queue.
    fn idx_add(&self, idx: u16, off: u16) -> u16 {
        wrap_add(*self.size, idx, off)
    }

    /// Helper method to calculate a negative offset for a given index, wrapping at
    /// the size of the queue.
    fn idx_sub(&self, idx: u16, off: u16) -> u16 {
        wrap_sub(*self.size, idx, off)
    }
}
impl<'a> QueueGuard<'a, CompQueueState> {
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
        if idx as u32 >= *self.size {
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

    fn take_avail(&mut self) -> bool {
        if let Some(avail) = self.state.inner.avail.checked_sub(1) {
            self.state.inner.avail = avail;
            true
        } else {
            // Make sure we kick the SQs when we have space available again
            self.state.inner.kick = true;
            false
        }
    }

    fn release_avail(&mut self) {
        if let Some(avail) = self.state.inner.avail.checked_add(1) {
            assert!(
                (avail as u32) < *self.size,
                "attempted to overflow CQ available size"
            );
            self.state.inner.avail = avail;
        } else {
            panic!("attempted to overflow CQ available");
        }
    }

    fn kick(&mut self) -> bool {
        std::mem::replace(&mut self.state.inner.kick, false)
    }
}

impl CompQueueState {
    /// Create a new `QueueState` for a Completion Queue
    fn new(size: u32) -> QueueState<CompQueueState> {
        QueueState::new(
            size,
            CompQueueState {
                avail: (size - 1) as u16,
                // As the device side, we start with our phase tag as asserted (1)
                // since the host side (VM) will create all the Completion Queue
                // entries with the phase initially zeroed out.
                phase: true,
                kick: false,
            },
        )
    }
}

impl SubQueueState {
    /// Create a new `QueueState` for a Submission Queue
    fn new(size: u32) -> QueueState<SubQueueState> {
        QueueState::new(size, SubQueueState())
    }
}
impl<'a> QueueGuard<'a, SubQueueState> {
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
    fn pop_head(&mut self) -> Option<u16> {
        if self.is_empty() {
            return None;
        } else {
            let old_head = self.state.head;
            self.state.head = self.idx_add(old_head, 1);
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
        if idx as u32 >= *self.size {
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

/// Type for manipulating Submission Queues.
#[derive(Debug)]
pub struct SubQueue {
    /// The ID of this Submission Queue.
    id: QueueId,

    /// The corresponding Completion Queue.
    cq: Arc<CompQueue>,

    /// Queue state such as the size and current head/tail entry pointers.
    state: QueueState<SubQueueState>,

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
        id: QueueId,
        cq: Arc<CompQueue>,
        size: u32,
        base: GuestAddr,
        mem: &MemCtx,
    ) -> Result<Arc<Self>, QueueCreateErr> {
        use std::collections::hash_map::Entry;
        Self::validate(id, base, size, mem)?;
        let sq =
            Arc::new(Self { id, cq, state: SubQueueState::new(size), base });
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
    pub fn notify_tail(&self, idx: u16) -> Result<(), QueueUpdateError> {
        let mut state = self.state.lock();
        state.push_tail_to(idx)
    }

    /// Returns the next entry off of the Queue or [`None`] if it is empty.
    pub fn pop(
        self: &Arc<SubQueue>,
        mem: &MemCtx,
    ) -> Option<(SubmissionQueueEntry, Permit, u16)> {
        // Attempt to reserve an entry on the Completion Queue
        let permit = self.cq.reserve_entry(&self)?;
        let mut state = self.state.lock();
        if let Some(idx) = state.pop_head() {
            let addr = self.base.offset::<SubmissionQueueEntry>(idx as usize);
            let ent = mem.read::<SubmissionQueueEntry>(addr);
            // XXX: handle a guest addr that becomes unmapped later
            ent.map(|ent| (ent, permit.promote(ent.cid()), idx))
        } else {
            // Drop lock on SQ before releasing permit (which locks CQ)
            drop(state);

            // No Submission Queue entry, so return the CQE permit
            permit.remit();
            None
        }
    }

    /// Returns the ID of this Submission Queue.
    pub(super) fn id(&self) -> QueueId {
        self.id
    }

    /// Annotate a CQE with data (ID and head index) from this SQ
    fn annotate_completion(&self, cqe: &mut CompletionQueueEntry) {
        let state = self.state.lock();
        cqe.sqid = self.id;
        cqe.sqhd = state.head();
    }

    /// Validates whether the given parameters may be used to create a
    /// Submission Queue object.
    fn validate(
        id: QueueId,
        base: GuestAddr,
        size: u32,
        mem: &MemCtx,
    ) -> Result<(), QueueCreateErr> {
        if (base.0 & PAGE_OFFSET as u64) != 0 {
            return Err(QueueCreateErr::InvalidBaseAddr);
        }
        let max = if id == ADMIN_QUEUE_ID {
            MAX_ADMIN_QUEUE_SIZE
        } else {
            MAX_QUEUE_SIZE
        };
        if size < MIN_QUEUE_SIZE || size > max {
            return Err(QueueCreateErr::InvalidSize);
        }
        let queue_size = size as usize * size_of::<SubmissionQueueEntry>();
        let region = mem.readable_region(&GuestRegion(base, queue_size));

        region.map(|_| ()).ok_or(QueueCreateErr::InvalidBaseAddr)
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
#[derive(Debug)]
pub struct CompQueue {
    /// The ID of this Completion Queue.
    id: QueueId,

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
        id: QueueId,
        iv: u16,
        size: u32,
        base: GuestAddr,
        hdl: pci::MsixHdl,
        mem: &MemCtx,
    ) -> Result<Self, QueueCreateErr> {
        Self::validate(id, base, size, mem)?;
        Ok(Self {
            id,
            iv,
            state: CompQueueState::new(size),
            base,
            hdl,
            sqs: Mutex::new(HashMap::new()),
        })
    }

    /// Attempt to move the Head entry pointer forward to the given index.
    pub fn notify_head(&self, idx: u16) -> Result<(), QueueUpdateError> {
        self.state.lock().pop_head_to(idx)
    }

    /// Fires an interrupt to the guest with the associated interrupt vector
    /// if the queue is not currently empty.
    pub fn fire_interrupt(&self) {
        let state = self.state.lock();
        if !state.is_empty() {
            self.hdl.fire(self.iv);
        }
    }

    /// Returns whether the SQs should be kicked due to no permits being
    /// available previously.
    ///
    /// If the value was true, it will also get reset to false.
    pub fn kick(&self) -> bool {
        self.state.lock().kick()
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
    ) -> Option<ProtoPermit> {
        let mut state = self.state.lock();
        if state.take_avail() {
            Some(ProtoPermit::new(self, sq))
        } else {
            // No more spots available.
            None
        }
    }

    /// Add a new entry to the Completion Queue while consuming a `Permit`.
    fn push(
        &self,
        comp: Completion,
        permit: Permit,
        sq: &SubQueue,
        mem: &MemCtx,
    ) {
        let mut cqe = CompletionQueueEntry::new(comp, permit.cid);
        sq.annotate_completion(&mut cqe);

        let mut guard = self.state.lock();
        let (idx, phase) = guard
            .push_tail()
            .expect("CQ should have available space for assigned permit");

        probes::nvme_cqe!(|| (self.id, idx, phase as u8));

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
        cqe.set_phase(!phase);
        mem.write(addr, &cqe);
        cqe.set_phase(phase);
        mem.write(addr, &cqe);
    }

    /// Validates whether the given parameters may be used to create a
    /// Completion Queue object.
    fn validate(
        id: QueueId,
        base: GuestAddr,
        size: u32,
        mem: &MemCtx,
    ) -> Result<(), QueueCreateErr> {
        if (base.0 & PAGE_OFFSET as u64) != 0 {
            return Err(QueueCreateErr::InvalidBaseAddr);
        }
        let max = if id == ADMIN_QUEUE_ID {
            MAX_ADMIN_QUEUE_SIZE
        } else {
            MAX_QUEUE_SIZE
        };
        if size < MIN_QUEUE_SIZE || size > max {
            return Err(QueueCreateErr::InvalidSize);
        }
        let queue_size = size as usize * size_of::<CompletionQueueEntry>();
        let region = mem.writable_region(&GuestRegion(base, queue_size));

        region.map(|_| ()).ok_or(QueueCreateErr::InvalidBaseAddr)
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

    /// The Submission Queue's ID (stored separately to avoid going through
    /// the Weak ref).
    sqid: u16,
}
impl ProtoPermit {
    fn new(cq: &Arc<CompQueue>, sq: &Arc<SubQueue>) -> Self {
        Self { cq: Arc::downgrade(cq), sq: Arc::downgrade(sq), sqid: sq.id }
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
            sqid: self.sqid,
            cid,
            completed: false,
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
#[derive(Debug)]
pub struct Permit {
    /// The corresponding Completion Queue for which we have a permit.
    cq: Weak<CompQueue>,

    /// The Submission Queue for which this entry is reserved.
    sq: Weak<SubQueue>,

    /// The Submission Queue ID the request came in on.
    sqid: u16,

    /// ID of command holding this permit.  Used to populate `cid` field in
    /// Completion Queue Entry.
    cid: u16,

    /// Track that `complete()` was actually called
    completed: bool,
}

impl Permit {
    /// Consume the permit by placing an entry into the Completion Queue.
    pub fn complete(mut self, comp: Completion, mem: Option<&MemCtx>) {
        assert!(!self.completed);
        self.completed = true;

        let cq = match self.cq.upgrade() {
            Some(cq) => cq,
            None => {
                // The CQ has since been deleted so no way to complete this
                // request nor to return the permit.
                debug_assert!(self.sq.upgrade().is_none());
                return;
            }
        };

        if let (Some(sq), Some(mem)) = (self.sq.upgrade(), mem) {
            cq.push(comp, self, &sq, mem);

            // TODO: should this be done here?
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

    /// Get the ID of the Submission Queue the command associated with this
    /// permit was submitted on.
    pub fn sqid(&self) -> u16 {
        self.sqid
    }

    /// Consume the permit by placing an entry into the Completion Queue.
    ///
    /// This is a simpler version of [Self::complete()] for testing purposes
    /// which does not require passing in the actual completion data.  It is
    /// only to be used for excercising the Submission and Completion Queues in
    /// unit tests.
    #[cfg(test)]
    fn test_complete(mut self, sq: &SubQueue, mem: &MemCtx) {
        self.completed = true;
        if let Some(cq) = self.cq.upgrade() {
            cq.push(Completion::success(), self, sq, mem);
        }
    }

    /// Some of the tests which to acquire Permit entries with no intent to
    /// drive them through to completion.  Allow them to bypass the
    /// ensure-this-permit-is-completed check in [`Drop`].
    #[cfg(test)]
    fn ignore(mut self) -> Self {
        self.completed = true;
        self
    }
}
impl Drop for Permit {
    fn drop(&mut self) {
        assert!(
            self.completed,
            "permit was dropped without calling complete()"
        );
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
mod tests {
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
        let read_base = GuestAddr(0);
        let write_base = GuestAddr(1024 * 1024);

        let acc_mem = machine.acc_mem.child(None);
        let mem = acc_mem.access().unwrap();

        // Admin queues must be less than 4K
        let cq = CompQueue::new(
            ADMIN_QUEUE_ID,
            0,
            1024,
            write_base,
            hdl.clone(),
            &mem,
        );
        assert!(matches!(cq, Ok(_)));
        let cq = CompQueue::new(
            ADMIN_QUEUE_ID,
            0,
            5 * 1024,
            write_base,
            hdl.clone(),
            &mem,
        );
        assert!(matches!(cq, Err(QueueCreateErr::InvalidSize)));

        // I/O queues must be less than 64K
        let cq = CompQueue::new(1, 0, 1024, write_base, hdl.clone(), &mem);
        assert!(matches!(cq, Ok(_)));
        let cq = CompQueue::new(1, 0, 65 * 1024, write_base, hdl.clone(), &mem);
        assert!(matches!(cq, Err(QueueCreateErr::InvalidSize)));

        // Neither must be less than 2
        let cq =
            CompQueue::new(ADMIN_QUEUE_ID, 0, 1, write_base, hdl.clone(), &mem);
        assert!(matches!(cq, Err(QueueCreateErr::InvalidSize)));
        let cq = CompQueue::new(1, 0, 1, write_base, hdl.clone(), &mem);
        assert!(matches!(cq, Err(QueueCreateErr::InvalidSize)));

        // Completion Queue's must be mapped to writable memory
        let cq =
            CompQueue::new(ADMIN_QUEUE_ID, 0, 2, read_base, hdl.clone(), &mem);
        assert!(matches!(cq, Err(QueueCreateErr::InvalidBaseAddr)));
        let cq = CompQueue::new(1, 0, 2, read_base, hdl, &mem);
        assert!(matches!(cq, Err(QueueCreateErr::InvalidBaseAddr)));

        Ok(())
    }

    #[test]
    fn create_sqs() -> Result<(), Error> {
        let machine = Machine::new_test()?;
        let hdl = pci::MsixHdl::new_test();
        let read_base = GuestAddr(0);
        let write_base = GuestAddr(1024 * 1024);

        let acc_mem = machine.acc_mem.child(None);
        let mem = acc_mem.access().unwrap();

        // Create corresponding CQs
        let admin_cq = Arc::new(
            CompQueue::new(
                ADMIN_QUEUE_ID,
                0,
                1024,
                write_base,
                hdl.clone(),
                &mem,
            )
            .unwrap(),
        );
        let io_cq = Arc::new(
            CompQueue::new(1, 0, 1024, write_base, hdl, &mem).unwrap(),
        );

        // Admin queues must be less than 4K
        let sq = SubQueue::new(
            ADMIN_QUEUE_ID,
            admin_cq.clone(),
            1024,
            read_base,
            &mem,
        );
        assert!(matches!(sq, Ok(_)));
        let sq = SubQueue::new(
            ADMIN_QUEUE_ID,
            admin_cq.clone(),
            5 * 1024,
            read_base,
            &mem,
        );
        assert!(matches!(sq, Err(QueueCreateErr::InvalidSize)));

        // I/O queues must be less than 64K
        let sq = SubQueue::new(1, io_cq.clone(), 1024, read_base, &mem);
        assert!(matches!(sq, Ok(_)));
        let sq = SubQueue::new(1, io_cq, 65 * 1024, read_base, &mem);
        assert!(matches!(sq, Err(QueueCreateErr::InvalidSize)));

        // Neither must be less than 2
        let sq =
            SubQueue::new(ADMIN_QUEUE_ID, admin_cq.clone(), 1, read_base, &mem);
        assert!(matches!(sq, Err(QueueCreateErr::InvalidSize)));
        let sq = SubQueue::new(1, admin_cq, 1, read_base, &mem);
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

        let acc_mem = machine.acc_mem.child(None);
        let mem = acc_mem.access().unwrap();

        // Create our queues
        let cq =
            Arc::new(CompQueue::new(1, 0, 4, write_base, hdl, &mem).unwrap());
        let sq =
            Arc::new(SubQueue::new(1, cq.clone(), 4, read_base, &mem).unwrap());

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
        while let Some((_, permit, _)) = sq.pop(&mem) {
            permit.test_complete(&sq, &mem);
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

        let acc_mem = machine.acc_mem.child(None);
        let mem = acc_mem.access().unwrap();

        // Create our queues
        // Purposely make the CQ smaller to test kicks
        let cq =
            Arc::new(CompQueue::new(1, 0, 2, write_base, hdl, &mem).unwrap());
        let sq =
            Arc::new(SubQueue::new(1, cq.clone(), 4, read_base, &mem).unwrap());

        // Replicate guest VM notifying us things were pushed to the SQ
        let mut sq_tail = 0;
        for _ in 0..sq.state.size - 1 {
            sq_tail = wrap_add(sq.state.size, sq_tail, 1);
            assert!(matches!(sq.notify_tail(sq_tail), Ok(_)));
        }

        // We should be able to pop based on how much space is in the CQ
        for _ in 0..cq.state.size - 1 {
            let pop = sq.pop(&mem);
            assert!(matches!(pop, Some(_)));

            // Complete these in the CQ (but note guest won't have acknowledged them yet)
            pop.unwrap().1.test_complete(&sq, &mem);
        }

        // But we can't pop anymore due to no more CQ space to reserve
        assert!(matches!(sq.pop(&mem), None));

        // The guest consuming things off the CQ should let free us
        assert!(matches!(cq.notify_head(1), Ok(_)));

        // Kick should've been set in the failed pop
        assert!(cq.kick());

        // We should have one more space now and should be able to pop 1 more
        assert!(matches!(
            sq.pop(&mem).map(|(_sub, permit, _idx)| {
                // ignore permit so it can be discarded when done
                permit.ignore()
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

        let acc_mem = machine.acc_mem.child(None);
        let mem = acc_mem.access().unwrap();

        // Create a pair of Completion and Submission Queues
        // with a random size. We purposefully give the CQ a smaller
        // size to exercise the "kick" conditions where we have some
        // request available in the SQ but can't pop it until there's
        // space available in the CQ.
        let mut rng = rand::thread_rng();
        let sq_size = rng.gen_range(512..2048);
        let cq =
            Arc::new(CompQueue::new(1, 0, 4, write_base, hdl, &mem).unwrap());
        let sq = Arc::new(
            SubQueue::new(1, cq.clone(), sq_size, read_base, &mem).unwrap(),
        );

        // We'll be generating a random number of submissions
        let submissions_rand = rng.gen_range(2..sq.state.size - 1);

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
                        if doorbell_cq.kick() {
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

                let child_acc = acc_mem.child(None);

                spawn(move || {
                    let mut submissions = 0;
                    let mem = child_acc.access().unwrap();

                    let mut rng = rand::thread_rng();
                    while let Ok(()) = worker_rx.recv() {
                        while let Some((_, cqe_permit, _)) = worker_sq.pop(&mem)
                        {
                            submissions += 1;

                            // Sleep for a bit to mimic actually doing
                            // some work before we complete the IO
                            sleep(Duration::from_micros(rng.gen_range(0..500)));

                            cqe_permit.test_complete(&worker_sq, &mem);

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
            sleep(Duration::from_micros(rng.gen_range(0..100)));
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
