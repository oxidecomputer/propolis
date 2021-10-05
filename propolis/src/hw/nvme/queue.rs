use std::marker::PhantomData;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use super::bits::{self, RawCompletion, RawSubmission};
use crate::common::*;
use crate::dispatch::DispCtx;
use crate::hw::pci;

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

    /// The combined current Head and Tail entry pointers.
    ///
    /// The least significant 16 bits represent the Head pointer while
    /// the next 16 represent the Tail pointer.
    ///
    /// Bit 32 is the current phase tag (for completion queues).
    ///
    /// The consumer of entries on a queue uses the current Head entry pointer
    /// to identify the next entry to be pulled off the queue.
    ///
    /// The submitter of entries to a queue uses the current Tail entry pointer
    /// to identify the next open queue entry space.
    ///
    /// The Phase Tag is used to identify to the host (VM) that a Completion
    /// entry is new. Flips every time the Tail entry pointer wraps around.
    ///
    /// See NVMe 1.0e Section 4.1 Submission Queue & Completion Queue Definition
    /// See NVMe 1.0e Section 4.5 Completion Queue Entry - Phase Tag (P)
    head_tail_phase: AtomicU64,

    /// Marker type to indicate what type of Queue we're modeling.
    _qt: PhantomData<QT>,
}

impl<QT> QueueState<QT> {
    /// Create a new `QueueState`
    fn new(size: u32) -> Self {
        assert!(size >= MIN_QUEUE_SIZE && size <= MAX_QUEUE_SIZE);
        Self { size, head_tail_phase: AtomicU64::new(1 << 32 /* phase tag */), _qt: PhantomData }
    }

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
    /// Attempt to return the Tail entry pointer and then move it forward by 1.
    ///
    /// If the queue is full this method returns [`None`].
    /// Otherwise, this method returns the current Tail entry pointer and then
    /// increments the Tail entry pointer by 1 (wrapping if necessary).
    fn push_tail(&self) -> Option<u16> {
        self.head_tail_phase
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |head_tail| {
                let (mut phase, tail, head) = (
                    (head_tail & (1 << 32)) != 0,
                    (head_tail >> 16) as u16,
                    head_tail as u16,
                );
                if self.is_full(head, tail) {
                    return None;
                }
                if tail as u32 + 1 >= self.size {
                    // We wrapped so flip phase
                    phase = !phase;
                }
                let new_tail = self.wrap_add(tail, 1);
                Some(
                    (phase as u64) << 32
                        | (new_tail as u64) << 16
                        | head as u64,
                )
            })
            .ok()
            .map(|old_head_tail| {
                let old_tail = (old_head_tail >> 16) as u16;
                old_tail
            })
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
    fn pop_head_to(&self, idx: u16) -> Result<(), &'static str> {
        if idx as u32 >= self.size {
            return Err("invalid index");
        }
        self.head_tail_phase
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |head_tail| {
                let (phase, tail, head) = (
                    (head_tail & (1 << 32)) != 0,
                    (head_tail >> 16) as u16,
                    head_tail as u16,
                );
                let pop_count = self.wrap_sub(idx, head);
                if pop_count > self.avail_occupied(head, tail) {
                    return None;
                }
                // Replace head with given idx
                Some((phase as u64) << 32 | (tail as u64) << 16 | idx as u64)
            })
            .map_err(|_| "index too far")
            .map(|_| ())
    }
}

impl QueueState<SubmissionQueueType> {
    /// Attempt to return the Head entry pointer and then move it forward by 1.
    ///
    /// If the queue is empty this method returns [`None`].
    /// Otherwise, this method returns the current Head entry pointer and then
    /// increments the Head entry pointer by 1 (wrapping if necessary).
    fn pop_head(&self) -> Option<u16> {
        self.head_tail_phase
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |head_tail| {
                let (tail, head) = ((head_tail >> 16) as u16, head_tail as u16);
                if self.is_empty(head, tail) {
                    return None;
                }
                let new_head = self.wrap_add(head, 1);
                Some((tail as u64) << 16 | new_head as u64)
            })
            .ok()
            .map(|old_head_tail| old_head_tail as u16)
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
    fn push_tail_to(&self, idx: u16) -> Result<(), &'static str> {
        if idx as u32 >= self.size {
            return Err("invalid index");
        }
        self.head_tail_phase
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |head_tail| {
                let (tail, head) = ((head_tail >> 16) as u16, head_tail as u16);
                let push_count = self.wrap_sub(idx, tail);
                if push_count > self.avail_empty(head, tail) {
                    return None;
                }
                // Replace tail with given idx
                Some((idx as u64) << 16 | head as u64)
            })
            .map_err(|_| "index too far")
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
        Ok(Self { id, cq, state: QueueState::new(size), base })
    }

    /// Attempt to move the Tail entry pointer forward to the given index.
    pub fn notify_tail(&self, idx: u16) -> Result<(), &'static str> {
        self.state.push_tail_to(idx)
    }

    /// Returns the next entry off of the Queue or [`None`] if it is empty.
    pub fn pop(&self, ctx: &DispCtx) -> Option<bits::RawSubmission> {
        if let Some(idx) = self.state.pop_head() {
            let mem = ctx.mctx.memctx();
            let ent: Option<RawSubmission> = mem.read(self.entry_addr(idx));
            // XXX: handle a guest addr that becomes unmapped later
            ent
        } else {
            None
        }
    }

    /// Returns the current Head entry pointer.
    pub fn head(&self) -> u16 {
        self.state.head_tail_phase.load(Ordering::SeqCst) as u16
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
        Ok(Self { iv, state: QueueState::new(size), base, hdl })
    }

    /// Attempt to move the Head entry pointer forward to the given index.
    pub fn notify_head(&self, idx: u16) -> Result<(), &'static str> {
        self.state.pop_head_to(idx)
    }

    /// Attempt to add a new entry to the Completion Queue.
    ///
    /// TODO: handle the case where the queue may be currently full.
    pub fn push(&self, entry: RawCompletion, ctx: &DispCtx) {
        if let Some(idx) = self.state.push_tail() {
            let mem = ctx.mctx.memctx();
            let addr = self.entry_addr(idx);
            mem.write(addr, &entry);
            // XXX: handle a guest addr that becomes unmapped later
            // XXX: figure out interrupts
        }
    }

    /// Returns the current Phase Tag bit.
    ///
    /// The current Phase Tag to identify to the host (VM) that a Completion
    /// entry is new. Flips every time the Tail entry pointer wraps around.
    ///
    /// See NVMe 1.0e Section 4.5 Completion Queue Entry - Phase Tag (P)
    pub fn phase(&self) -> u16 {
        (self.state.head_tail_phase.load(Ordering::SeqCst) >> 32) as u16
    }

    /// Fires an interrupt to the guest with the associated interrupt vector
    /// if the queue is not currently empty.
    pub fn fire_interrupt(&self, ctx: &DispCtx) {
        let head_tail = self.state.head_tail_phase.load(Ordering::SeqCst);
        let (tail, head) = ((head_tail >> 16) as u16, head_tail as u16);
        if !self.state.is_empty(head, tail) {
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
