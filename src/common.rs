use std::ops::{Add, BitAnd};

pub struct ReadOp<'a> {
    pub offset: usize,
    pub buf: &'a mut [u8],
}
impl<'a> ReadOp<'a> {
    pub fn new(offset: usize, buf: &'a mut [u8]) -> Self {
        Self { offset, buf }
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

#[derive(Copy, Clone, Debug)]
pub struct GuestAddr(pub u64);

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
