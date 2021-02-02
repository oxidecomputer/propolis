use std::ops::{Add, BitAnd};

pub struct ReadOp<'a> {
    pub offset: usize,
    pub buf: &'a mut [u8],
}
impl<'a> ReadOp<'a> {
    pub fn new(offset: usize, buf: &'a mut [u8]) -> Self {
        Self { offset, buf }
    }

    /// Consumes `size` bytes of the buffer, pushing the offset forward by that
    /// figure, and shrinking the buffer (from the front) by the same amount.
    pub fn consume(mut self, size: usize) -> Self {
        assert!(size <= self.buf.len());
        self.offset += size;
        self.buf = &mut self.buf[size..];
        self
    }

    pub fn truncate(mut self, size: usize) -> Self {
        let target = size.min(self.buf.len());
        self.buf = &mut self.buf[..target];
        self
    }

    pub fn limit(&self) -> Option<usize> {
        self.offset.checked_add(self.buf.len())
    }

    /// Fill the contained buffer with `val` byte
    pub fn fill(&mut self, val: u8) {
        for b in self.buf.iter_mut() {
            *b = val;
        }
    }
}

pub struct WriteOp<'a> {
    pub offset: usize,
    pub buf: &'a [u8],
}
impl<'a> WriteOp<'a> {
    pub fn new(offset: usize, buf: &'a [u8]) -> Self {
        Self { offset, buf }
    }
}

pub enum RWOp<'a, 'b> {
    Read(&'a mut ReadOp<'b>),
    Write(&'a WriteOp<'b>),
}
impl RWOp<'_, '_> {
    pub fn offset(&self) -> usize {
        match self {
            RWOp::Read(ro) => ro.offset,
            RWOp::Write(wo) => wo.offset,
        }
    }
    pub fn len(&self) -> usize {
        match self {
            RWOp::Read(ro) => ro.buf.len(),
            RWOp::Write(wo) => wo.buf.len(),
        }
    }
    pub fn buf(&self) -> &[u8] {
        match self {
            RWOp::Read(ro) => ro.buf,
            RWOp::Write(wo) => wo.buf,
        }
    }
}

#[derive(Copy, Clone, Debug)]
pub struct GuestAddr(pub u64);
#[derive(Copy, Clone, Debug)]
pub struct GuestRegion(pub GuestAddr, pub usize);

impl Add<usize> for GuestAddr {
    type Output = Self;

    fn add(self, rhs: usize) -> Self::Output {
        Self(self.0 + rhs as u64)
    }
}
impl BitAnd<usize> for GuestAddr {
    type Output = Self;

    fn bitand(self, rhs: usize) -> Self::Output {
        Self(self.0 & rhs as u64)
    }
}

pub const PAGE_SIZE: usize = 0x1000;
pub const PAGE_OFFSET: usize = 0xfff;
pub const PAGE_MASK: usize = usize::MAX - PAGE_OFFSET;
pub const PAGE_SHIFT: usize = 12;

pub fn round_up_p2(val: usize, to: usize) -> usize {
    assert!(to.is_power_of_two());
    assert!(to != 0);

    val.checked_add(to - 1).unwrap() & !(to - 1)
}
