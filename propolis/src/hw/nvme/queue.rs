use super::bits::{self, RawCompletion, RawSubmission};
use crate::common::*;
use crate::dispatch::DispCtx;
use crate::hw::pci;

use thiserror::Error;

const MIN_SIZE: u32 = 2;
const MAX_SIZE: u32 = 1 << 16;

struct QueueState {
    size: u32,
    head: u16,
    tail: u16,
}
impl QueueState {
    fn new(size: u32, head: u16, tail: u16) -> Self {
        assert!(size >= MIN_SIZE && size <= MAX_SIZE);
        Self { size, head, tail }
    }
    fn is_empty(&self) -> bool {
        // 4.1.1 Empty Queue
        //
        // The queue is Empty when the Head entry pointer equals the Tail entry
        // pointer.
        self.head == self.tail
    }
    fn is_full(&self) -> bool {
        // 4.1.2 Full Queue
        //
        // The queue is Full when the Head equals one more than the Tail.  The
        // number of entries in a queue when full is one less than the queue
        // size.
        (self.head > 0 && self.tail == (self.head - 1))
            || (self.head == 0 && self.tail == (self.size - 1) as u16)
    }

    /// Calculate a positive offset for a given index, wrapping at the size of
    /// the queue.
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
    /// Calculate a negative offset for a given index, wrapping at the size of
    /// the queue.
    fn wrap_sub(&self, idx: u16, off: u16) -> u16 {
        debug_assert!((idx as u32) < self.size);
        debug_assert!((off as u32) < self.size);

        if off > idx {
            ((idx as u32 + self.size) - off as u32) as u16
        } else {
            idx - off
        }
    }

    /// How many slots are empty between the tail and the head
    fn avail_empty(&self) -> u16 {
        self.wrap_sub(self.wrap_sub(self.head, 1), self.tail)
    }
    /// How many slots are occupied between the head and the tail
    fn avail_occupied(&self) -> u16 {
        self.wrap_sub(self.tail, self.head)
    }

    fn push_tail(&mut self) -> Option<(u16, bool)> {
        if self.is_full() {
            None
        } else {
            let result = Some(self.tail);
            self.tail = self.wrap_add(self.tail, 1);
            result.map(|r| (r, self.tail as u32 > self.size))
        }
    }
    fn pop_head(&mut self) -> Option<u16> {
        if self.is_empty() {
            None
        } else {
            let result = Some(self.head);
            self.head = self.wrap_add(self.head, 1);
            result
        }
    }

    fn push_tail_to(&mut self, idx: u16) -> Result<(), &'static str> {
        if idx as u32 >= self.size {
            return Err("invalid index");
        }
        let push_count = self.wrap_sub(idx, self.tail);
        if push_count > self.avail_empty() {
            return Err("index too far");
        }
        self.tail = idx;
        Ok(())
    }
    fn pop_head_to(&mut self, idx: u16) -> Result<(), &'static str> {
        if idx as u32 >= self.size {
            return Err("invalid index");
        }
        let pop_count = self.wrap_sub(idx, self.head);
        if pop_count > self.avail_occupied() {
            return Err("index too far");
        }
        self.head = idx;
        Ok(())
    }
}

#[derive(Error, Debug)]
pub enum QueueCreateErr {
    #[error("invalid base address")]
    InvalidBaseAddr,
    #[error("invalid size")]
    InvalidSize,
}

pub struct SubQueue {
    id: u16,
    cqid: u16,
    state: QueueState,
    base: GuestAddr,
}

impl SubQueue {
    pub fn new(
        id: u16,
        cqid: u16,
        size: u32,
        base: GuestAddr,
        ctx: &DispCtx,
    ) -> Result<Self, QueueCreateErr> {
        Self::validate(base, size, ctx)?;
        Ok(Self { id, cqid, state: QueueState::new(size, 0, 0), base })
    }
    pub fn notify_tail(&mut self, idx: u16) -> Result<(), &'static str> {
        self.state.push_tail_to(idx)
    }
    pub fn pop(&mut self, ctx: &DispCtx) -> Option<bits::RawSubmission> {
        if let Some(idx) = self.state.pop_head() {
            let mem = ctx.mctx.memctx();
            let ent: Option<RawSubmission> = mem.read(self.entry_addr(idx));
            // XXX: handle a guest addr that becomes unmapped later
            ent
        } else {
            None
        }
    }
    pub fn head(&self) -> u16 {
        self.state.head
    }
    pub fn id(&self) -> u16 {
        self.id
    }
    pub fn cqid(&self) -> u16 {
        self.cqid
    }
    fn entry_addr(&self, idx: u16) -> GuestAddr {
        let res = self.base.0
            + idx as u64 * std::mem::size_of::<RawSubmission>() as u64;
        GuestAddr(res)
    }
    fn validate(
        base: GuestAddr,
        size: u32,
        ctx: &DispCtx,
    ) -> Result<(), QueueCreateErr> {
        if (base.0 & PAGE_OFFSET as u64) != 0 {
            return Err(QueueCreateErr::InvalidBaseAddr);
        }
        if size < MIN_SIZE || size > MAX_SIZE {
            return Err(QueueCreateErr::InvalidSize);
        }
        let queue_size =
            size as usize * std::mem::size_of::<bits::RawSubmission>();
        let memctx = ctx.mctx.memctx();
        let region = memctx.readable_region(&GuestRegion(base, queue_size));

        region.map(|_| ()).ok_or(QueueCreateErr::InvalidBaseAddr)
    }
}

pub struct CompQueue {
    iv: u16,
    state: QueueState,
    base: GuestAddr,
    phase: u16,
    hdl: pci::MsixHdl,
}

impl CompQueue {
    pub fn new(
        iv: u16,
        size: u32,
        base: GuestAddr,
        ctx: &DispCtx,
        hdl: pci::MsixHdl,
    ) -> Result<Self, QueueCreateErr> {
        Self::validate(base, size, ctx)?;
        Ok(Self {
            iv,
            state: QueueState::new(size, 0, 0),
            base,
            phase: 1,
            hdl,
        })
    }
    pub fn notify_head(&mut self, idx: u16) -> Result<(), &'static str> {
        self.state.pop_head_to(idx)
    }
    pub fn push(&mut self, entry: RawCompletion, ctx: &DispCtx) {
        if let Some((idx, wrapped)) = self.state.push_tail() {
            let mem = ctx.mctx.memctx();
            let addr = self.entry_addr(idx);
            mem.write(addr, &entry);
            if wrapped {
                self.phase = !self.phase;
            }
            // XXX: handle a guest addr that becomes unmapped later
            // XXX: figure out interrupts
        }
    }
    pub fn phase(&self) -> u16 {
        self.phase
    }
    pub fn is_empty(&self) -> bool {
        self.state.is_empty()
    }
    pub fn fire_interrupt(&self, ctx: &DispCtx) {
        if !self.is_empty() {
            self.hdl.fire(self.iv, ctx);
        }
    }
    fn entry_addr(&self, idx: u16) -> GuestAddr {
        let res = self.base.0
            + idx as u64 * std::mem::size_of::<RawCompletion>() as u64;
        GuestAddr(res)
    }
    fn validate(
        base: GuestAddr,
        size: u32,
        ctx: &DispCtx,
    ) -> Result<(), QueueCreateErr> {
        if (base.0 & PAGE_OFFSET as u64) != 0 {
            return Err(QueueCreateErr::InvalidBaseAddr);
        }
        if size < MIN_SIZE || size > MAX_SIZE {
            return Err(QueueCreateErr::InvalidSize);
        }
        let queue_size =
            size as usize * std::mem::size_of::<bits::RawSubmission>();
        let memctx = ctx.mctx.memctx();
        let region = memctx.writable_region(&GuestRegion(base, queue_size));

        region.map(|_| ()).ok_or(QueueCreateErr::InvalidBaseAddr)
    }
}
