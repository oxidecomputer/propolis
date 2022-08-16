use std::collections::HashMap;
use std::sync::{Arc, Mutex, Weak};

use super::bits::{self, RawCompletion, RawSubmission};
use super::cmds::Completion;
use crate::common::*;
use crate::hw::pci;
use crate::migrate::MigrateStateError;
use crate::vmm::MemCtx;

use thiserror::Error;

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

    /// Whether the CQ should kick its SQs due to no permits being available previously.
    ///
    /// One may only pop something off the SQ if there's at least one space available in
    /// the corresponding CQ. If there isn't, we set the kick flag.
    kick: bool,
}

/// Submission Queue State
#[derive(Debug)]
struct SubQueueState {
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
}

/// Helper for manipulating Completion/Submission Queues
///
/// The type parameter `QT` is used to constrain the set of
/// methods exposed based on whether the queue in question
/// is a Completion or Submission queue. Use either
/// `CompletionQueueType` or `SubmissionQueueType`.
#[derive(Debug)]
struct QueueState<QS> {
    /// The size of the queue in question.
    ///
    /// See NVMe 1.0e Section 4.1.3 Queue Size
    size: u32,

    /// The actual queue state that gets updated during the normal course of operation.
    ///
    /// Either `CompQueueState` for a Completion Queue or
    /// a `SubQueueState` for a Submission Queue.
    inner: Mutex<QS>,
}

impl<QS> QueueState<QS> {
    /// Returns if the queue is currently empty with the given head and tail pointers.
    ///
    /// A queue is empty when the Head entry pointer equals the Tail entry pointer.
    ///
    /// See: NVMe 1.0e Section 4.1.1 Empty Queue
    fn is_empty(&self, head: u16, tail: u16) -> bool {
        head == tail
    }

    /// Returns if the queue is currently full with the given head and tail pointers.
    ///
    /// The queue is full when the Head entry pointer equals one more than the Tail
    /// entry pointer. The number of entries in a queue will always be 1 less than
    /// the queue size.
    ///
    /// See: NVMe 1.0e Section 4.1.2 Full Queue
    fn is_full(&self, head: u16, tail: u16) -> bool {
        (head > 0 && tail == (head - 1))
            || (head == 0 && tail == (self.size - 1) as u16)
    }

    /// Helper method to calculate a positive offset for a given index, wrapping at
    //// the size of the queue.
    fn wrap_add(&self, idx: u16, off: u16) -> u16 {
        debug_assert!((idx as u32) < self.size);
        debug_assert!((off as u32) < self.size);

        let res = idx as u32 + off as u32;
        if res >= self.size {
            (res - self.size) as u16
        } else {
            res as u16
        }
    }

    /// Helper method to calculate a negative offset for a given index, wrapping at
    /// the size of the queue.
    fn wrap_sub(&self, idx: u16, off: u16) -> u16 {
        debug_assert!((idx as u32) < self.size);
        debug_assert!((off as u32) < self.size);

        if off > idx {
            ((idx as u32 + self.size) - off as u32) as u16
        } else {
            idx - off
        }
    }
}

impl QueueState<CompQueueState> {
    /// Create a new `QueueState` for a Completion Queue
    fn new_completion_state(size: u32) -> QueueState<CompQueueState> {
        assert!(size >= MIN_QUEUE_SIZE && size <= MAX_QUEUE_SIZE);
        // As the device side, we start with our phase tag as asserted (1)
        // as the host side (VM) will create all the Completion Queue entries
        // with the phase initially zeroed out.
        let inner = CompQueueState {
            head: 0,
            tail: 0,
            avail: (size - 1) as u16,
            phase: true,
            kick: false,
        };
        Self { size, inner: Mutex::new(inner) }
    }

    /// Attempt to return the Tail entry pointer and then move it forward by 1.
    ///
    /// If the queue is full this method returns [`None`].
    /// Otherwise, this method returns the current Tail entry pointer and then
    /// increments the Tail entry pointer by 1 (wrapping if necessary).
    fn push_tail(&self) -> Option<u16> {
        let mut state = self.inner.lock().unwrap();
        if self.is_full(state.head, state.tail) {
            return None;
        }
        if state.tail as u32 + 1 >= self.size {
            // We wrapped so flip phase
            state.phase = !state.phase;
        }
        let old_tail = state.tail;
        state.tail = self.wrap_add(old_tail, 1);
        Some(old_tail)
    }

    /// How many slots are occupied between the head and the tail i.e., how
    /// many entries can we read from the queue currently.
    fn avail_occupied(&self, head: u16, tail: u16) -> u16 {
        self.wrap_sub(tail, head)
    }

    /// Attempt to move the Head entry pointer forward to the given index.
    ///
    /// The given index must be less than the size of the queue. The queue
    /// must have enough occupied slots otherwise we return an error.
    /// Conceptually this method indicates some entries have been consumed
    /// from the queue.
    fn pop_head_to(&self, idx: u16) -> Result<(), QueueUpdateError> {
        if idx as u32 >= self.size {
            return Err(QueueUpdateError::InvalidEntry);
        }
        let mut state = self.inner.lock().unwrap();
        let pop_count = self.wrap_sub(idx, state.head);
        if pop_count > self.avail_occupied(state.head, state.tail) {
            return Err(QueueUpdateError::TooManyEntries);
        }
        // Replace head with given idx and update the number of available slots
        state.head = idx;
        state.avail += pop_count;

        Ok(())
    }
}

impl QueueState<SubQueueState> {
    /// Create a new `QueueState` for a Submission Queue
    fn new_submission_state(size: u32) -> QueueState<SubQueueState> {
        assert!(size >= MIN_QUEUE_SIZE && size <= MAX_QUEUE_SIZE);
        let inner = SubQueueState { head: 0, tail: 0 };
        Self { size, inner: Mutex::new(inner) }
    }

    /// Attempt to return the Head entry pointer and then move it forward by 1.
    ///
    /// If the queue is empty this method returns [`None`].
    /// Otherwise, this method returns the current Head entry pointer and then
    /// increments the Head entry pointer by 1 (wrapping if necessary).
    fn pop_head(&self) -> Option<u16> {
        let mut state = self.inner.lock().unwrap();
        if self.is_empty(state.head, state.tail) {
            return None;
        }
        let old_head = state.head;
        state.head = self.wrap_add(old_head, 1);
        Some(old_head)
    }

    /// How many slots are empty between the tail and the head i.e., how many
    /// entries can we write to the queue currently.
    fn avail_empty(&self, head: u16, tail: u16) -> u16 {
        self.wrap_sub(self.wrap_sub(head, 1), tail)
    }

    /// Attempt to move the Tail entry pointer forward to the given index.
    ///
    /// The given index must be less than the size of the queue. The queue
    /// must have enough empty slots available otherwise we return an error.
    /// Conceptually this method indicates new entries have been added to the
    /// queue.
    fn push_tail_to(&self, idx: u16) -> Result<(), QueueUpdateError> {
        if idx as u32 >= self.size {
            return Err(QueueUpdateError::InvalidEntry);
        }
        let mut state = self.inner.lock().unwrap();
        let push_count = self.wrap_sub(idx, state.tail);
        if push_count > self.avail_empty(state.head, state.tail) {
            return Err(QueueUpdateError::TooManyEntries);
        }
        // Replace tail with given idx
        state.tail = idx;

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
        let sq = Arc::new(Self {
            id,
            cq,
            state: QueueState::new_submission_state(size),
            base,
        });
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
        self.state.push_tail_to(idx)
    }

    /// Returns the next entry off of the Queue or [`None`] if it is empty.
    pub fn pop(
        self: &Arc<SubQueue>,
        mem: &MemCtx,
    ) -> Option<(bits::RawSubmission, CompQueueEntryPermit)> {
        // Attempt to reserve an entry on the Completion Queue
        let cqe_permit = self.cq.reserve_entry(self.clone())?;
        if let Some(idx) = self.state.pop_head() {
            let ent: Option<RawSubmission> = mem.read(self.entry_addr(idx));
            // XXX: handle a guest addr that becomes unmapped later
            ent.map(|ent| (ent, cqe_permit))
        } else {
            // No Submission Queue entry, so return the CQE permit
            cqe_permit.remit();
            None
        }
    }

    /// Returns the current Head entry pointer.
    fn head(&self) -> u16 {
        let state = self.state.inner.lock().unwrap();
        state.head
    }

    /// Returns the ID of this Submission Queue.
    fn id(&self) -> QueueId {
        self.id
    }

    /// Returns the corresponding [`GuestAddr`] for a given entry in
    /// the Submission Queue.
    fn entry_addr(&self, idx: u16) -> GuestAddr {
        let res = self.base.0
            + idx as u64 * std::mem::size_of::<RawSubmission>() as u64;
        GuestAddr(res)
    }

    /// Validates whether the given parameters may be used to create
    /// a Submission Queue object.
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
        let queue_size =
            size as usize * std::mem::size_of::<bits::RawSubmission>();
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
            state: QueueState::new_completion_state(size),
            base,
            hdl,
            sqs: Mutex::new(HashMap::new()),
        })
    }

    /// Attempt to move the Head entry pointer forward to the given index.
    pub fn notify_head(&self, idx: u16) -> Result<(), QueueUpdateError> {
        self.state.pop_head_to(idx)
    }

    /// Fires an interrupt to the guest with the associated interrupt vector
    /// if the queue is not currently empty.
    pub fn fire_interrupt(&self) {
        let state = self.state.inner.lock().unwrap();
        if !self.state.is_empty(state.head, state.tail) {
            self.hdl.fire(self.iv);
        }
    }

    /// Returns whether the SQ's should be kicked due to no permits being available previously.
    ///
    /// If the value was true, it will also get reset to false.
    pub fn kick(&self) -> bool {
        let mut state = self.state.inner.lock().unwrap();
        std::mem::replace(&mut state.kick, false)
    }

    /// Returns the number of SQ's associated with this Completion Queue.
    pub fn associated_sqs(&self) -> usize {
        let sqs = self.sqs.lock().unwrap();
        sqs.len()
    }

    /// Attempt to reserve an entry in the Completion Queue.
    ///
    /// An entry permit allows the user to push onto the Completion Queue.
    fn reserve_entry(
        self: &Arc<Self>,
        sq: Arc<SubQueue>,
    ) -> Option<CompQueueEntryPermit> {
        let mut state = self.state.inner.lock().unwrap();
        // No more spots available
        if state.avail == 0 {
            // Make sure we kick the SQ's when we have space available again
            state.kick = true;

            None
        } else {
            // Otherwise claim a spot
            state.avail -= 1;

            Some(CompQueueEntryPermit {
                cq: Arc::downgrade(self),
                sq: Arc::downgrade(&sq),
            })
        }
    }

    /// Add a new entry to the Completion Queue while consuming a `CompQueueEntryPermit`.
    fn push(
        &self,
        _permit: CompQueueEntryPermit,
        entry: RawCompletion,
        mem: &MemCtx,
    ) {
        // Since we have a permit, there should always be at least
        // one space in the queue and this unwrap shouldn't fail.
        let idx = self.state.push_tail().unwrap();
        let addr = self.entry_addr(idx);
        mem.write(addr, &entry);
        // XXX: handle a guest addr that becomes unmapped later
    }

    /// Returns the current Phase Tag bit.
    ///
    /// The current Phase Tag to identify to the host (VM) that a Completion
    /// entry is new. Flips every time the Tail entry pointer wraps around.
    ///
    /// See NVMe 1.0e Section 4.5 Completion Queue Entry - Phase Tag (P)
    fn phase(&self) -> u16 {
        let state = self.state.inner.lock().unwrap();
        state.phase as u16
    }

    /// Returns the corresponding [`GuestAddr`] for a given entry in
    /// the Completion Queue.
    fn entry_addr(&self, idx: u16) -> GuestAddr {
        let res = self.base.0
            + idx as u64 * std::mem::size_of::<RawCompletion>() as u64;
        GuestAddr(res)
    }

    /// Validates whether the given parameters may be used to create
    /// a Completion Queue object.
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
        let queue_size =
            size as usize * std::mem::size_of::<bits::RawSubmission>();
        let region = mem.writable_region(&GuestRegion(base, queue_size));

        region.map(|_| ()).ok_or(QueueCreateErr::InvalidBaseAddr)
    }

    pub(super) fn export(&self) -> migrate::NvmeCompQueueV1 {
        let inner = self.state.inner.lock().unwrap();
        migrate::NvmeCompQueueV1 {
            id: self.id,
            size: self.state.size,
            head: inner.head,
            tail: inner.tail,
            avail: inner.avail,
            phase: inner.phase,
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

        let mut inner = self.state.inner.lock().unwrap();
        inner.head = state.head;
        inner.tail = state.tail;
        inner.avail = state.avail;
        inner.phase = state.phase;

        Ok(())
    }
}

/// A type which allows pushing a Completion Entry onto the Completion Queue.
#[derive(Debug)]
pub struct CompQueueEntryPermit {
    /// The corresponding Completion Queue for which we have a permit.
    cq: Weak<CompQueue>,

    /// The Submission Queue for which this entry is reserved.
    sq: Weak<SubQueue>,
}

impl CompQueueEntryPermit {
    /// Consume the permit by placing an entry into the Completion Queue.
    pub fn push_completion(
        self,
        cid: u16,
        comp: Completion,
        mem: Option<&MemCtx>,
    ) {
        let cq = match self.cq.upgrade() {
            Some(cq) => cq,
            None => {
                // The CQ has since been deleted so no way to complete this
                // request nor to return the permit.
                assert!(self.sq.upgrade().is_none());
                return;
            }
        };

        if let (Some(sq), Some(mem)) = (self.sq.upgrade(), mem) {
            let completion = bits::RawCompletion {
                dw0: comp.dw0,
                rsvd: 0,
                sqhd: sq.head(),
                sqid: sq.id(),
                cid,
                status_phase: comp.status | cq.phase(),
            };

            cq.push(self, completion, mem);

            // TODO: should this be done here?
            cq.fire_interrupt();
        } else {
            // The SQ has since been deleted (so the request has already
            // implicitly been aborted by the prior Delete Queue command) or
            // the device currently lacks access to guest memory.
            //
            // Just make sure we return the permit
            self.remit();
        }
    }

    /// Consume the permit by placing an entry into the Completion Queue.
    ///
    /// This is a simpler version of `CompQueueEntryPermit::push_completion`
    /// just for testing purposes that doesn't require passing in the actual
    /// completion data. Meant just for excercising the Submission & Completion
    /// Queues in unit tests.
    #[cfg(test)]
    fn push_completion_test(self, mem: &MemCtx) {
        if let Some(cq) = self.cq.upgrade() {
            cq.push(self, bits::RawCompletion::default(), mem);
        }
    }

    /// Return the permit without having actually used it.
    ///
    /// Frees up the space for someone else to grab it via `CompQueue::reserve_entry`.
    fn remit(self) {
        if let Some(cq) = self.cq.upgrade() {
            let mut state = cq.state.inner.lock().unwrap();
            state.avail += 1;
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
mod tests {
    use rand::Rng;

    use super::*;

    use crate::{common::GuestAddr, instance::Instance};
    use std::io::Error;
    use std::thread::{sleep, spawn};
    use std::time::Duration;

    #[test]
    fn create_cqs() -> Result<(), Error> {
        let instance = Instance::new_test()?;
        let hdl = pci::MsixHdl::new_test();
        let read_base = GuestAddr(0);
        let write_base = GuestAddr(1024 * 1024);

        let acc_mem = instance.lock().machine().acc_mem.child();
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
        let cq = CompQueue::new(1, 0, 2, read_base, hdl.clone(), &mem);
        assert!(matches!(cq, Err(QueueCreateErr::InvalidBaseAddr)));

        Ok(())
    }

    #[test]
    fn create_sqs() -> Result<(), Error> {
        let instance = Instance::new_test()?;
        let hdl = pci::MsixHdl::new_test();
        let read_base = GuestAddr(0);
        let write_base = GuestAddr(1024 * 1024);

        let acc_mem = instance.lock().machine().acc_mem.child();
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
            CompQueue::new(1, 0, 1024, write_base, hdl.clone(), &mem).unwrap(),
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
        let sq = SubQueue::new(1, io_cq.clone(), 65 * 1024, read_base, &mem);
        assert!(matches!(sq, Err(QueueCreateErr::InvalidSize)));

        // Neither must be less than 2
        let sq =
            SubQueue::new(ADMIN_QUEUE_ID, admin_cq.clone(), 1, read_base, &mem);
        assert!(matches!(sq, Err(QueueCreateErr::InvalidSize)));
        let sq = SubQueue::new(1, admin_cq.clone(), 1, read_base, &mem);
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
        let instance = Instance::new_test()?;
        let hdl = pci::MsixHdl::new_test();
        let read_base = GuestAddr(0);
        let write_base = GuestAddr(1024 * 1024);

        let acc_mem = instance.lock().machine().acc_mem.child();
        let mem = acc_mem.access().unwrap();

        // Create our queues
        let cq = Arc::new(
            CompQueue::new(1, 0, 4, write_base, hdl.clone(), &mem).unwrap(),
        );
        let sq =
            Arc::new(SubQueue::new(1, cq.clone(), 4, read_base, &mem).unwrap());

        // Replicate guest VM notifying us things were pushed to the SQ
        let mut sq_tail = 0;
        for _ in 0..sq.state.size - 1 {
            sq_tail = sq.state.wrap_add(sq_tail, 1);
            // These should all succeed
            assert!(matches!(sq.notify_tail(sq_tail), Ok(_)));
        }

        // But anything more should fail
        sq_tail = sq.state.wrap_add(sq_tail, 1);
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
        while let Some((_, permit)) = sq.pop(&mem) {
            permit.push_completion_test(&mem);
        }

        // Replicate guest VM notifying us things were consumed off the CQ
        let mut cq_head = 0;
        for _ in 0..sq.state.size - 1 {
            cq_head = cq.state.wrap_add(cq_head, 1);
            // These should all succeed
            assert!(matches!(cq.notify_head(cq_head), Ok(_)));
        }

        // There's nothing else to pop so this should fail
        cq_head = cq.state.wrap_add(cq_head, 1);
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
        let instance = Instance::new_test()?;
        let hdl = pci::MsixHdl::new_test();
        let read_base = GuestAddr(0);
        let write_base = GuestAddr(1024 * 1024);

        let acc_mem = instance.lock().machine().acc_mem.child();
        let mem = acc_mem.access().unwrap();

        // Create our queues
        // Purposely make the CQ smaller to test kicks
        let cq = Arc::new(
            CompQueue::new(1, 0, 2, write_base, hdl.clone(), &mem).unwrap(),
        );
        let sq =
            Arc::new(SubQueue::new(1, cq.clone(), 4, read_base, &mem).unwrap());

        // Replicate guest VM notifying us things were pushed to the SQ
        let mut sq_tail = 0;
        for _ in 0..sq.state.size - 1 {
            sq_tail = sq.state.wrap_add(sq_tail, 1);
            assert!(matches!(sq.notify_tail(sq_tail), Ok(_)));
        }

        // We should be able to pop based on how much space is in the CQ
        for _ in 0..cq.state.size - 1 {
            let pop = sq.pop(&mem);
            assert!(matches!(pop, Some(_)));

            // Complete these in the CQ (but note guest won't have acknowledged them yet)
            pop.unwrap().1.push_completion_test(&mem);
        }

        // But we can't pop anymore due to no more CQ space to reserve
        assert!(matches!(sq.pop(&mem), None));

        // The guest consuming things off the CQ should let free us
        assert!(matches!(cq.notify_head(1), Ok(_)));

        // Kick should've been set in the failed pop
        assert!(cq.kick());

        // We should have one more space now and should be able to pop 1 more
        assert!(matches!(sq.pop(&mem), Some(_)));

        Ok(())
    }

    #[test]
    fn push_pop() -> Result<(), Error> {
        let instance = Instance::new_test()?;
        let hdl = pci::MsixHdl::new_test();
        let read_base = GuestAddr(0);
        let write_base = GuestAddr(1024 * 1024);

        let acc_mem = instance.lock().machine().acc_mem.child();
        let mem = acc_mem.access().unwrap();

        // Create a pair of Completion and Submission Queues
        // with a random size. We purposefully give the CQ a smaller
        // size to exercise the "kick" conditions where we have some
        // request available in the SQ but can't pop it until there's
        // space available in the CQ.
        let mut rng = rand::thread_rng();
        let sq_size = rng.gen_range(512..2048);
        let cq = Arc::new(
            CompQueue::new(1, 0, 4, write_base, hdl.clone(), &mem).unwrap(),
        );
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
        let (doorbell_cq, doorbell_sq) = (cq.clone(), sq.clone());
        let doorbell_handler = spawn(move || {
            // Keep track of the "host" side CQ head and SQ tail as
            // we receive "doorbell" hits.
            let mut cq_head = 0;
            let mut sq_tail = 0;
            loop {
                match doorbell_rx.recv() {
                    Ok(Doorbell::Cq(n)) => {
                        cq_head = doorbell_cq.state.wrap_add(cq_head, n);
                        assert!(matches!(
                            doorbell_cq.notify_head(cq_head),
                            Ok(_)
                        ));
                        if doorbell_cq.kick() {
                            assert!(workers_tx.send(()).is_ok());
                        }
                    }
                    Ok(Doorbell::Sq(n)) => {
                        sq_tail = doorbell_sq.state.wrap_add(sq_tail, n);
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
        let io_workers = (0..4)
            .map(|_| {
                let worker_rx = workers_rx.clone();
                let worker_sq = sq.clone();
                let worker_comp_tx = comp_tx.clone();

                let child_acc = acc_mem.child();

                spawn(move || {
                    let mut submissions = 0;
                    let mem = child_acc.access().unwrap();

                    let mut rng = rand::thread_rng();
                    loop {
                        match worker_rx.recv() {
                            Ok(()) => {
                                while let Some((_, cqe_permit)) =
                                    worker_sq.pop(&mem)
                                {
                                    submissions += 1;

                                    // Sleep for a bit to mimic actually doing
                                    // some work before we complete the IO
                                    sleep(Duration::from_micros(
                                        rng.gen_range(0..500),
                                    ));

                                    cqe_permit.push_completion_test(&mem);

                                    // Signal "guest" side of completion handler
                                    assert!(worker_comp_tx.send(()).is_ok());
                                }
                            }
                            Err(_) => break,
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
