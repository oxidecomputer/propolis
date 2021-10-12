use std::marker::PhantomData;
use std::sync::atomic::{AtomicU64, Ordering};
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
#[derive(Debug)]
enum CompletionQueueType {}

/// Marker type to indicate a Submission Queue.
#[derive(Debug)]
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

        /// Whether the CQ should kick its SQs due to no permits being available previously.
        ///
        /// One may only pop something off the SQ if there's at least one space available in
        /// the corresponding CQ. If there isn't, we set the kick flag.
        kick: bool = 49;
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
#[derive(Debug)]
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
#[derive(Debug)]
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
    fn head(&self) -> u16 {
        SubQueueState(self.state.inner.load(Ordering::SeqCst)).head()
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
#[derive(Debug)]
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
        Ok(Self {
            iv,
            state: QueueState::new_completion_state(size),
            base,
            hdl,
        })
    }

    /// Attempt to move the Head entry pointer forward to the given index.
    pub fn notify_head(&self, idx: u16) -> Result<(), QueueUpdateError> {
        self.state.pop_head_to(idx)
    }

    /// Fires an interrupt to the guest with the associated interrupt vector
    /// if the queue is not currently empty.
    pub fn fire_interrupt(&self, ctx: &DispCtx) {
        let state = CompQueueState(self.state.inner.load(Ordering::SeqCst));
        if !self.state.is_empty(state.head(), state.tail()) {
            self.hdl.fire(self.iv, ctx);
        }
    }

    /// Returns whether the SQ's should be kicked due to no permits being available previously.
    pub fn kick(&self) -> bool {
        self.state
            .inner
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |state| {
                let state = CompQueueState(state);
                Some(state.with_kick(false).0)
            })
            .ok()
            .map(|old_state| CompQueueState(old_state).kick())
            // This unwrap is fine as our fetch_update never returns None
            .unwrap()
    }

    /// Attempt to reserve an entry in the Completion Queue.
    ///
    /// An entry permit allows the user to push onto the Completion Queue.
    fn reserve_entry(
        self: &Arc<Self>,
        sq: Arc<SubQueue>,
    ) -> Option<CompQueueEntryPermit> {
        let old_state = self
            .state
            .inner
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |state| {
                let state = CompQueueState(state);
                let avail = state.avail();
                // No more spots available
                if avail == 0 {
                    // Make sure we kick the SQ's when we have space available again
                    Some(state.with_kick(true).0)
                } else {
                    // Otherwise claim a spot
                    Some(state.with_avail(avail - 1).0)
                }
            })
            // Unwrap here is fine as our fetch_update never returns None.
            .unwrap();
        let old_state = CompQueueState(old_state);
        if old_state.avail() == 0 {
            None
        } else {
            Some(CompQueueEntryPermit { cq: self.clone(), sq })
        }
    }

    /// Add a new entry to the Completion Queue while consuming a `CompQueueEntryPermit`.
    fn push(
        &self,
        _permit: CompQueueEntryPermit,
        entry: RawCompletion,
        ctx: &DispCtx,
    ) {
        // Since we have a permit, there should always be at least
        // one space in the queue and this unwrap shouldn't fail.
        let idx = self.state.push_tail().unwrap();
        let mem = ctx.mctx.memctx();
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
        CompQueueState(self.state.inner.load(Ordering::SeqCst)).phase() as u16
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
#[derive(Debug)]
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

        let cq = self.cq.clone();

        cq.push(self, completion, ctx);

        // TODO: should this be done here?
        cq.fire_interrupt(ctx);
    }

    /// Consume the permit by placing an entry into the Completion Queue.
    ///
    /// This is a simpler version of `CompQueueEntryPermit::push_completion`
    /// just for testing purposes that doesn't require passing in the actual
    /// completion data. Meant just for excercising the Submission & Completion
    /// Queues in unit tests.
    #[cfg(test)]
    fn push_completion_test(self, ctx: &DispCtx) {
        let cq = self.cq.clone();
        cq.push(self, bits::RawCompletion::default(), ctx);
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
            // Unwrap here is fine as our fetch_update never returns None
            .unwrap();
    }
}

#[cfg(test)]
mod tests {
    use rand::Rng;

    use super::*;

    use crate::{
        common::GuestAddr,
        dispatch::{SharedCtx, SyncCtx},
        instance::Instance,
    };
    use std::assert_matches::assert_matches;
    use std::io::Error;
    use std::thread::{sleep, spawn};
    use std::time::Duration;

    macro_rules! dispctx (
        ($ctx:ident, $instance:expr) => {
            let mut sctx =
            SyncCtx::standalone(SharedCtx::create(&$instance.disp));
            let $ctx = sctx.dispctx();
        };
    );

    #[test]
    fn create_cqs() -> Result<(), Error> {
        let instance = Instance::new_test(None)?;
        dispctx!(ctx, instance);

        let hdl = pci::MsixHdl::new_test();
        let read_base = GuestAddr(0);
        let write_base = GuestAddr(1024 * 1024);

        // Admin queues must be less than 4K
        let cq = CompQueue::new(
            ADMIN_QUEUE_ID,
            0,
            1024,
            write_base,
            &ctx,
            hdl.clone(),
        );
        assert_matches!(cq, Ok(_));
        let cq = CompQueue::new(
            ADMIN_QUEUE_ID,
            0,
            5 * 1024,
            write_base,
            &ctx,
            hdl.clone(),
        );
        assert_matches!(cq, Err(QueueCreateErr::InvalidSize));

        // I/O queues must be less than 64K
        let cq = CompQueue::new(1, 0, 1024, write_base, &ctx, hdl.clone());
        assert_matches!(cq, Ok(_));
        let cq = CompQueue::new(1, 0, 65 * 1024, write_base, &ctx, hdl.clone());
        assert_matches!(cq, Err(QueueCreateErr::InvalidSize));

        // Neither must be less than 2
        let cq =
            CompQueue::new(ADMIN_QUEUE_ID, 0, 1, write_base, &ctx, hdl.clone());
        assert_matches!(cq, Err(QueueCreateErr::InvalidSize));
        let cq = CompQueue::new(1, 0, 1, write_base, &ctx, hdl.clone());
        assert_matches!(cq, Err(QueueCreateErr::InvalidSize));

        // Completion Queue's must be mapped to writable memory
        let cq =
            CompQueue::new(ADMIN_QUEUE_ID, 0, 2, read_base, &ctx, hdl.clone());
        assert_matches!(cq, Err(QueueCreateErr::InvalidBaseAddr));
        let cq = CompQueue::new(1, 0, 2, read_base, &ctx, hdl.clone());
        assert_matches!(cq, Err(QueueCreateErr::InvalidBaseAddr));

        Ok(())
    }

    #[test]
    fn create_sqs() -> Result<(), Error> {
        let instance = Instance::new_test(None)?;
        dispctx!(ctx, instance);

        let hdl = pci::MsixHdl::new_test();
        let read_base = GuestAddr(0);
        let write_base = GuestAddr(1024 * 1024);

        // Create corresponding CQs
        let admin_cq = Arc::new(
            CompQueue::new(
                ADMIN_QUEUE_ID,
                0,
                1024,
                write_base,
                &ctx,
                hdl.clone(),
            )
            .unwrap(),
        );
        let io_cq = Arc::new(
            CompQueue::new(1, 0, 1024, write_base, &ctx, hdl.clone()).unwrap(),
        );

        // Admin queues must be less than 4K
        let sq = SubQueue::new(
            ADMIN_QUEUE_ID,
            admin_cq.clone(),
            1024,
            read_base,
            &ctx,
        );
        assert_matches!(sq, Ok(_));
        let sq = SubQueue::new(
            ADMIN_QUEUE_ID,
            admin_cq.clone(),
            5 * 1024,
            read_base,
            &ctx,
        );
        assert_matches!(sq, Err(QueueCreateErr::InvalidSize));

        // I/O queues must be less than 64K
        let sq = SubQueue::new(1, io_cq.clone(), 1024, read_base, &ctx);
        assert_matches!(sq, Ok(_));
        let sq = SubQueue::new(1, io_cq.clone(), 65 * 1024, read_base, &ctx);
        assert_matches!(sq, Err(QueueCreateErr::InvalidSize));

        // Neither must be less than 2
        let sq =
            SubQueue::new(ADMIN_QUEUE_ID, admin_cq.clone(), 1, read_base, &ctx);
        assert_matches!(sq, Err(QueueCreateErr::InvalidSize));
        let sq = SubQueue::new(1, admin_cq.clone(), 1, read_base, &ctx);
        assert_matches!(sq, Err(QueueCreateErr::InvalidSize));

        // Completion Queue's must be mapped to readable memory
        let sq = SubQueue::new(
            ADMIN_QUEUE_ID,
            admin_cq.clone(),
            2,
            write_base,
            &ctx,
        );
        assert_matches!(sq, Err(QueueCreateErr::InvalidBaseAddr));
        let sq = SubQueue::new(1, admin_cq.clone(), 2, write_base, &ctx);
        assert_matches!(sq, Err(QueueCreateErr::InvalidBaseAddr));

        Ok(())
    }

    #[test]
    fn push_failures() -> Result<(), Error> {
        let instance = Instance::new_test(None)?;
        dispctx!(ctx, instance);
        let hdl = pci::MsixHdl::new_test();
        let read_base = GuestAddr(0);
        let write_base = GuestAddr(1024 * 1024);

        // Create our queues
        let cq = Arc::new(
            CompQueue::new(1, 0, 4, write_base, &ctx, hdl.clone()).unwrap(),
        );
        let sq =
            Arc::new(SubQueue::new(1, cq.clone(), 4, read_base, &ctx).unwrap());

        // Replicate guest VM notifying us things were pushed to the SQ
        let mut sq_tail = 0;
        for _ in 0..sq.state.size - 1 {
            sq_tail = sq.state.wrap_add(sq_tail, 1);
            // These should all succeed
            assert_matches!(sq.notify_tail(sq_tail), Ok(_));
        }

        // But anything more should fail
        sq_tail = sq.state.wrap_add(sq_tail, 1);
        assert_matches!(
            sq.notify_tail(sq_tail),
            Err(QueueUpdateError::TooManyEntries)
        );

        // Also anything that falls outside the boundaries (i.e. we didn't wrap properly)
        assert_matches!(
            sq.notify_tail(sq.state.size as u16),
            Err(QueueUpdateError::InvalidEntry)
        );

        // Now pop those SQ items and complete them in the CQ
        while let Some((_, permit)) = sq.pop(&ctx) {
            permit.push_completion_test(&ctx);
        }

        // Replicate guest VM notifying us things were consumed off the CQ
        let mut cq_head = 0;
        for _ in 0..sq.state.size - 1 {
            cq_head = cq.state.wrap_add(cq_head, 1);
            // These should all succeed
            assert_matches!(cq.notify_head(cq_head), Ok(_));
        }

        // There's nothing else to pop so this should fail
        cq_head = cq.state.wrap_add(cq_head, 1);
        assert_matches!(
            cq.notify_head(cq_head),
            Err(QueueUpdateError::TooManyEntries)
        );

        // Also anything that falls outside the boundaries (i.e. we didn't wrap properly)
        assert_matches!(
            cq.notify_head(cq.state.size as u16),
            Err(QueueUpdateError::InvalidEntry)
        );

        Ok(())
    }

    #[test]
    fn cq_kicks() -> Result<(), Error> {
        let instance = Instance::new_test(None)?;
        dispctx!(ctx, instance);
        let hdl = pci::MsixHdl::new_test();
        let read_base = GuestAddr(0);
        let write_base = GuestAddr(1024 * 1024);

        // Create our queues
        // Purposely make the CQ smaller to test kicks
        let cq = Arc::new(
            CompQueue::new(1, 0, 2, write_base, &ctx, hdl.clone()).unwrap(),
        );
        let sq =
            Arc::new(SubQueue::new(1, cq.clone(), 4, read_base, &ctx).unwrap());

        // Replicate guest VM notifying us things were pushed to the SQ
        let mut sq_tail = 0;
        for _ in 0..sq.state.size - 1 {
            sq_tail = sq.state.wrap_add(sq_tail, 1);
            assert_matches!(sq.notify_tail(sq_tail), Ok(_));
        }

        // We should be able to pop based on how much space is in the CQ
        for _ in 0..cq.state.size - 1 {
            let pop = sq.pop(&ctx);
            assert_matches!(pop, Some(_));

            // Complete these in the CQ (but note guest won't have acknowledged them yet)
            pop.unwrap().1.push_completion_test(&ctx);
        }

        // But we can't pop anymore due to no more CQ space to reserve
        assert_matches!(sq.pop(&ctx), None);

        // The guest consuming things off the CQ should let free us
        assert_matches!(cq.notify_head(1), Ok(_));

        // Kick should've been set in the failed pop
        assert!(cq.kick());

        // We should have one more space now and should be able to pop 1 more
        assert_matches!(sq.pop(&ctx), Some(_));

        Ok(())
    }

    #[test]
    fn push_pop() -> Result<(), Error> {
        let instance = Instance::new_test(None)?;
        dispctx!(ctx, instance);
        let hdl = pci::MsixHdl::new_test();
        let read_base = GuestAddr(0);
        let write_base = GuestAddr(1024 * 1024);

        // Create a pair of Completion and Submission Queues
        // with a random size. We purposefully give the CQ a smaller
        // size to exercise the "kick" conditions where we have some
        // request available in the SQ but can't pop it until there's
        // space available in the CQ.
        let mut rng = rand::thread_rng();
        let sq_size = rng.gen_range(512..2048);
        let cq = Arc::new(
            CompQueue::new(1, 0, 4, write_base, &ctx, hdl.clone()).unwrap(),
        );
        let sq = Arc::new(
            SubQueue::new(1, cq.clone(), sq_size, read_base, &ctx).unwrap(),
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
                        assert_matches!(
                            doorbell_cq.notify_head(cq_head),
                            Ok(_)
                        );
                        if doorbell_cq.kick() {
                            assert!(workers_tx.send(()).is_ok());
                        }
                    }
                    Ok(Doorbell::Sq(n)) => {
                        sq_tail = doorbell_sq.state.wrap_add(sq_tail, n);
                        // The "doorbell" was rung and so let's have the SQ
                        // update its internal state before poking the workers
                        assert_matches!(
                            doorbell_sq.notify_tail(sq_tail),
                            Ok(_)
                        );
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
                let worker_instance = instance.clone();
                let worker_rx = workers_rx.clone();
                let worker_sq = sq.clone();
                let worker_comp_tx = comp_tx.clone();
                spawn(move || {
                    dispctx!(ctx, worker_instance);
                    let mut rng = rand::thread_rng();
                    let mut submissions = 0;
                    loop {
                        match worker_rx.recv() {
                            Ok(()) => {
                                while let Some((_, cqe_permit)) =
                                    worker_sq.pop(&ctx)
                                {
                                    submissions += 1;

                                    // Sleep for a bit to mimic actually doing some
                                    // work before we complete the IO
                                    sleep(Duration::from_micros(
                                        rng.gen_range(0..500),
                                    ));

                                    cqe_permit.push_completion_test(&ctx);

                                    // Now signal the "guest" side of the completion handler
                                    assert!(worker_comp_tx.send(()).is_ok());
                                }
                            }
                            Err(_) => break submissions,
                        }
                    }
                })
            })
            .collect::<Vec<_>>();

        // Create a thread to "consume" things off the Completion Queue.
        // This simulates the host reacting to our CQ pushes and "ringing" the
        // CQ doorbell. At the end, it returns how many completion were handled.
        // Regardless, it'll stop after the number of completions is at least
        // as many as the number of submissions we decided to generate.
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

        // Make sure the number of submission we recevied matched the
        // number we generated and completed
        assert_eq!(submissions, submissions_rand);
        assert_eq!(submissions, completions);

        Ok(())
    }
}
