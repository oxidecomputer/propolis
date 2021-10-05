use std::marker::PhantomData;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use super::bits::{self, RawCompletion, RawSubmission};
use super::cmds::Completion;
use crate::common::*;
use crate::dispatch::DispCtx;
use crate::hw::pci;

use bitstruct::bitstruct;
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

/// Marker type to indicate a Completion Queue.
enum CompletionQueueType {}

/// Marker type to indicate a Submission Queue.
enum SubmissionQueueType {}

bitstruct! {
    /// Completion Queue State we update atomically.
    struct CompQueueState(pub u64) {
        /// The Queue Head entry pointer.
        ///
        /// The consumer of entries on a queue uses the current Head entry pointer
        /// to identify the next entry to be pulled off the queue.
        ///
        /// See NVMe 1.0e Section 4.1 Submission Queue & Completion Queue Definition
        head: u16 = 0..16;

        /// The Queue Tail entry pointer.
        ///
        /// The submitter of entries to a queue uses the current Tail entry pointer
        /// to identify the next open queue entry space.
        ///
        /// See NVMe 1.0e Section 4.1 Submission Queue & Completion Queue Definition
        tail: u16 = 16..32;

        /// Number of entries that are available for use.
        ///
        /// Starts off as queue size - 1 and gets decremented for each corresponding
        /// Submission Queue entry we begin servicing.
        avail: u16 = 32..48;

        /// The current phase tag.
        ///
        /// The Phase Tag is used to identify to the host (VM) that a Completion
        /// entry is new. Flips every time the Tail entry pointer wraps around.
        ///
        /// See NVMe 1.0e Section 4.5 Completion Queue Entry - Phase Tag (P)
        phase: bool = 48;
    }
}

bitstruct! {
    /// Submission Queue State we update atomically.
    struct SubQueueState(pub u64) {
        /// The Queue Head entry pointer.
        ///
        /// The consumer of entries on a queue uses the current Head entry pointer
        /// to identify the next entry to be pulled off the queue.
        ///
        /// See NVMe 1.0e Section 4.1 Submission Queue & Completion Queue Definition
        head: u16 = 0..16;

        /// The Queue Tail entry pointer.
        ///
        /// The submitter of entries to a queue uses the current Tail entry pointer
        /// to identify the next open queue entry space.
        ///
        /// See NVMe 1.0e Section 4.1 Submission Queue & Completion Queue Definition
        tail: u16 = 16..32;
    }
}

/// Helper for manipulating Completion/Submission Queues
///
/// The type parameter `QT` is used to constrain the set of
/// methods exposed based on whether the queue in question
/// is a Completion or Submission queue. Use either
/// `CompletionQueueType` or `SubmissionQueueType`.
struct QueueState<QT> {
    /// The size of the queue in question.
    ///
    /// See NVMe 1.0e Section 4.1.3 Queue Size
    size: u32,

    /// The actual atomic state that gets updated during the normal course of operation.
    ///
    /// Either `CompQueueState` for a Completion Queue or
    /// a `SubQueueState` for a Submission Queue.
    inner: AtomicU64,

    /// Marker type to indicate what type of Queue we're modeling.
    _qt: PhantomData<QT>,
}

impl<QT> QueueState<QT> {
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

impl QueueState<CompletionQueueType> {
    /// Create a new `QueueState` for a Completion Queue
    fn new_completion_state(size: u32) -> QueueState<CompletionQueueType> {
        assert!(size >= MIN_QUEUE_SIZE && size <= MAX_QUEUE_SIZE);
        // As the device side, we start with our phase tag as asserted (1)
        // as the host side (VM) will create all the Completion Queue entries
        // with the phase initially zeroed out.
        let inner =
            CompQueueState(0).with_phase(true).with_avail((size - 1) as u16);
        Self { size, inner: AtomicU64::new(inner.0), _qt: PhantomData }
    }

    /// Attempt to return the Tail entry pointer and then move it forward by 1.
    ///
    /// If the queue is full this method returns [`None`].
    /// Otherwise, this method returns the current Tail entry pointer and then
    /// increments the Tail entry pointer by 1 (wrapping if necessary).
    fn push_tail(&self) -> Option<u16> {
        self.inner
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |state| {
                let mut state = CompQueueState(state);
                if self.is_full(state.head(), state.tail()) {
                    return None;
                }
                if state.tail() as u32 + 1 >= self.size {
                    // We wrapped so flip phase
                    state.set_phase(!state.phase());
                }
                let new_tail = self.wrap_add(state.tail(), 1);
                Some(state.with_tail(new_tail).0)
            })
            .ok()
            .map(|state| CompQueueState(state).tail())
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
        self.inner
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |state| {
                let state = CompQueueState(state);
                let pop_count = self.wrap_sub(idx, state.head());
                if pop_count > self.avail_occupied(state.head(), state.tail()) {
                    return None;
                }
                // Replace head with given idx and update the number of available slots
                let avail = state.avail() + pop_count;
                Some(state.with_head(idx).with_avail(avail).0)
            })
            .map_err(|_| QueueUpdateError::TooManyEntries)
            .map(|_| ())
    }
}

impl QueueState<SubmissionQueueType> {
    /// Create a new `QueueState` for a Submission Queue
    fn new_submission_state(size: u32) -> QueueState<SubmissionQueueType> {
        assert!(size >= MIN_QUEUE_SIZE && size <= MAX_QUEUE_SIZE);
        let inner = SubQueueState(0);
        Self { size, inner: AtomicU64::new(inner.0), _qt: PhantomData }
    }

    /// Attempt to return the Head entry pointer and then move it forward by 1.
    ///
    /// If the queue is empty this method returns [`None`].
    /// Otherwise, this method returns the current Head entry pointer and then
    /// increments the Head entry pointer by 1 (wrapping if necessary).
    fn pop_head(&self) -> Option<u16> {
        self.inner
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |state| {
                let state = SubQueueState(state);
                if self.is_empty(state.head(), state.tail()) {
                    return None;
                }
                let new_head = self.wrap_add(state.head(), 1);
                Some(state.with_head(new_head).0)
            })
            .ok()
            .map(|state| SubQueueState(state).head())
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
        self.inner
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |state| {
                let state = SubQueueState(state);
                let push_count = self.wrap_sub(idx, state.tail());
                if push_count > self.avail_empty(state.head(), state.tail()) {
                    return None;
                }
                // Replace tail with given idx
                Some(state.with_tail(idx).0)
            })
            .map_err(|_| QueueUpdateError::TooManyEntries)
            .map(|_| ())
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
pub struct SubQueue {
    /// The ID of this Submission Queue.
    id: QueueId,

    /// The corresponding Completion Queue.
    cq: Arc<CompQueue>,

    /// Queue state such as the size and current head/tail entry pointers.
    state: QueueState<SubmissionQueueType>,

    /// The [`GuestAddr`] at which the Queue is mapped.
    base: GuestAddr,
}

impl SubQueue {
    /// Create a Submission Queue object backed by the guest memory at the
    /// given base address.
    pub fn new(
        id: QueueId,
        cq: Arc<CompQueue>,
        size: u32,
        base: GuestAddr,
        ctx: &DispCtx,
    ) -> Result<Self, QueueCreateErr> {
        Self::validate(id, base, size, ctx)?;
        Ok(Self { id, cq, state: QueueState::new_submission_state(size), base })
    }

    /// Attempt to move the Tail entry pointer forward to the given index.
    pub fn notify_tail(&self, idx: u16) -> Result<(), QueueUpdateError> {
        self.state.push_tail_to(idx)
    }

    /// Returns the next entry off of the Queue or [`None`] if it is empty.
    pub fn pop(
        self: &Arc<SubQueue>,
        ctx: &DispCtx,
    ) -> Option<(bits::RawSubmission, CompQueueEntryPermit)> {
        // Attempt to reserve an entry on the Completion Queue
        let cqe_permit = self.cq.reserve_entry(self.clone())?;
        // Reserve some space on the Completion Queue
        if let Some(idx) = self.state.pop_head() {
            let mem = ctx.mctx.memctx();
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
    pub fn head(&self) -> u16 {
        SubQueueState(self.state.inner.load(Ordering::SeqCst)).head()
    }

    /// Returns the ID of this Submission Queue.
    pub fn id(&self) -> QueueId {
        self.id
    }

    /// Returns the corresponding Completion Queue
    pub fn cq(&self) -> Arc<CompQueue> {
        self.cq.clone()
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
        ctx: &DispCtx,
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
        let memctx = ctx.mctx.memctx();
        let region = memctx.readable_region(&GuestRegion(base, queue_size));

        region.map(|_| ()).ok_or(QueueCreateErr::InvalidBaseAddr)
    }
}

/// Type for manipulating Completion Queues.
pub struct CompQueue {
    /// The Interrupt Vector used to signal to the host (VM) upon pushing
    /// entries onto the Completion Queue.
    iv: u16,

    /// Queue state such as the size and current head/tail entry pointers.
    state: QueueState<CompletionQueueType>,

    /// Flag set when when've run out of `CompQueueEntryPermit`'s to give out.
    ///
    /// Polled on CQ Doorbell events so that we kick the SQ's that wanted a CompQueueEntryPermit.
    kick: AtomicBool,

    /// The [`GuestAddr`] at which the Queue is mapped.
    base: GuestAddr,

    /// MSI-X object associated with PCIe device to signal host (VM).
    hdl: pci::MsixHdl,
}

impl CompQueue {
    /// Creates a Completion Queue object backed by the guest memory at the
    /// given base address.
    pub fn new(
        id: QueueId,
        iv: u16,
        size: u32,
        base: GuestAddr,
        ctx: &DispCtx,
        hdl: pci::MsixHdl,
    ) -> Result<Self, QueueCreateErr> {
        Self::validate(id, base, size, ctx)?;
        Ok(Self {
            iv,
            state: QueueState::new_completion_state(size),
            kick: AtomicBool::new(false),
            base,
            hdl,
        })
    }

    /// Attempt to move the Head entry pointer forward to the given index.
    pub fn notify_head(&self, idx: u16) -> Result<(), QueueUpdateError> {
        self.state.pop_head_to(idx)
    }

    /// Attempt to add a new entry to the Completion Queue.
    ///
    /// TODO: handle the case where the queue may be currently full.
    fn push(&self, entry: RawCompletion, ctx: &DispCtx) {
        if let Some(idx) = self.state.push_tail() {
            let mem = ctx.mctx.memctx();
            let addr = self.entry_addr(idx);
            mem.write(addr, &entry);
            // XXX: handle a guest addr that becomes unmapped later
            // XXX: figure out interrupts
        }
    }

    /// Attempt to reserve an entry in the Completion Queue.
    ///
    /// An entry permit allows the user to push onto the Completion Queue.
    pub fn reserve_entry(
        self: &Arc<Self>,
        sq: Arc<SubQueue>,
    ) -> Option<CompQueueEntryPermit> {
        self.state
            .inner
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |state| {
                let state = CompQueueState(state);
                let avail = state.avail();
                // No more spots available
                if avail == 0 {
                    // Make sure we kick the SQ's when we have space available again
                    self.kick.store(true, Ordering::SeqCst);
                    return None;
                }
                // Otherwise claim a spot
                Some(state.with_avail(avail - 1).0)
            })
            .ok()
            .map(|_| CompQueueEntryPermit { cq: self.clone(), sq })
    }

    /// Returns whether the SQ's should be kicked due to no permits being available previously.
    pub fn kick(&self) -> bool {
        self.kick.swap(false, Ordering::SeqCst)
    }

    /// Returns the current Phase Tag bit.
    ///
    /// The current Phase Tag to identify to the host (VM) that a Completion
    /// entry is new. Flips every time the Tail entry pointer wraps around.
    ///
    /// See NVMe 1.0e Section 4.5 Completion Queue Entry - Phase Tag (P)
    pub fn phase(&self) -> u16 {
        CompQueueState(self.state.inner.load(Ordering::SeqCst)).phase() as u16
    }

    /// Fires an interrupt to the guest with the associated interrupt vector
    /// if the queue is not currently empty.
    pub fn fire_interrupt(&self, ctx: &DispCtx) {
        let state = CompQueueState(self.state.inner.load(Ordering::SeqCst));
        if !self.state.is_empty(state.head(), state.tail()) {
            self.hdl.fire(self.iv, ctx);
        }
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
        ctx: &DispCtx,
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
        let memctx = ctx.mctx.memctx();
        let region = memctx.writable_region(&GuestRegion(base, queue_size));

        region.map(|_| ()).ok_or(QueueCreateErr::InvalidBaseAddr)
    }
}

/// A type which allows pushing a Completion Entry onto the Completion Queue.
pub struct CompQueueEntryPermit {
    /// The corresponding Completion Queue for which we have a permit.
    cq: Arc<CompQueue>,

    /// The Submission Queue for which this entry is reserved.
    sq: Arc<SubQueue>,
}

impl CompQueueEntryPermit {
    /// Consume the permit by placing an entry into the Completion Queue.
    pub fn push_completion(self, cid: u16, comp: Completion, ctx: &DispCtx) {
        let completion = bits::RawCompletion {
            dw0: comp.dw0,
            rsvd: 0,
            sqhd: self.sq.head(),
            sqid: self.sq.id(),
            cid,
            status_phase: comp.status | self.cq.phase(),
        };

        self.cq.push(completion, ctx);

        // TODO: should this be done here?
        self.cq.fire_interrupt(ctx);
    }

    /// Return the permit without having actually used it.
    ///
    /// Frees up the space for someone else to grab it via `CompQueue::reserve_entry`.
    fn remit(self) {
        self.cq
            .state
            .inner
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |state| {
                let state = CompQueueState(state);
                let avail = state.avail();
                Some(state.with_avail(avail + 1).0)
            })
            .unwrap();
    }
}
