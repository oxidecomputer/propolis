// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::ops::{Add, BitAnd};
use std::ops::{Bound::*, RangeBounds};
use std::slice::SliceIndex;

use crate::vmm::SubMapping;

fn numeric_bounds(
    bound: impl RangeBounds<usize>,
    len: usize,
) -> (usize, usize) {
    match (bound.start_bound(), bound.end_bound()) {
        (Unbounded, Unbounded) => (0, len),
        (Unbounded, Included(i)) => {
            assert!(*i < len);
            (0, i.checked_add(1).unwrap())
        }
        (Unbounded, Excluded(e)) => {
            assert!(*e < len);
            (0, *e)
        }

        (Included(i), Unbounded) => {
            assert!(*i < len);
            (*i, len)
        }
        (Included(si), Included(ei)) => {
            assert!(*si < len);
            assert!(*ei < len);
            assert!(*si <= *ei);
            (*si, ei.checked_add(1).unwrap())
        }
        (Included(si), Excluded(ee)) => {
            assert!(*si < len);
            assert!(*ee <= len);
            assert!(*si <= *ee);
            (*si, *ee)
        }
        (Excluded(_), _) => {
            panic!("Exclude start_bound not supported");
        }
    }
}

enum ROInner<'a> {
    Buf(&'a mut [u8]),
    Map(SubMapping<'a>),
}

/// Represents an abstract requested read operation.
///
/// Exposes an API with various "write" methods, which fulfill the request.
pub struct ReadOp<'a> {
    inner: ROInner<'a>,
    offset: usize,
    write_offset: usize,
}

impl<'a> ReadOp<'a> {
    /// Initializes a new read operation from a mapping.
    ///
    /// # Arguments
    ///
    /// - `op_offset`: An auxiliary offset stored within the operation,
    /// identifying the region which should be accessed to populate `mapping`.
    /// - `mapping`: A mapping which represents the "sink" of the read operation.
    pub fn from_mapping(op_offset: usize, mapping: SubMapping<'a>) -> Self {
        Self {
            inner: ROInner::Map(mapping),
            offset: op_offset,
            write_offset: 0,
        }
    }

    /// Initializes a new read operation from a buffer.
    ///
    /// # Arguments
    ///
    /// - `op_offset`: An auxiliary offset stored within the operation,
    /// identifying the region which should be accessed to populate `buf`.
    /// - `buffer`: A buffer which represents the "sink" of the read operation.
    pub fn from_buf(op_offset: usize, buffer: &'a mut [u8]) -> Self {
        Self { inner: ROInner::Buf(buffer), offset: op_offset, write_offset: 0 }
    }

    /// Constructs a child read operation from within an existing read
    /// operation.
    ///
    /// # Arguments
    ///
    /// - `op_offset`: Offset of the child operation. Does not need to correlate
    /// to the `parent` operation's offset.
    /// - `parent`: The operation from which this operation is being split.
    /// - `range`: The location within the parent operation to be moved
    /// to the child.
    pub fn new_child<'b, R>(
        op_offset: usize,
        parent: &'a mut ReadOp,
        range: R,
    ) -> ReadOp<'b>
    where
        'a: 'b,
        R: RangeBounds<usize> + SliceIndex<[u8], Output = [u8]>,
    {
        match &mut parent.inner {
            ROInner::Buf(b) => ReadOp {
                inner: ROInner::Buf(&mut b[range]),
                offset: op_offset,
                write_offset: 0,
            },
            ROInner::Map(m) => {
                let (start, end) = numeric_bounds(range, m.len());
                let len = end - start;
                let m = m.subregion(start, len).unwrap();
                ReadOp {
                    inner: ROInner::Map(m),
                    offset: op_offset,
                    write_offset: 0,
                }
            }
        }
    }

    pub fn len(&self) -> usize {
        match &self.inner {
            ROInner::Buf(b) => b.len(),
            ROInner::Map(m) => m.len(),
        }
    }
    pub fn avail(&self) -> usize {
        self.len().checked_sub(self.write_offset).unwrap()
    }
    pub fn offset(&self) -> usize {
        self.offset
    }

    pub fn write_u8(&mut self, val: u8) {
        self.write_bytes(&val.to_le_bytes()[..]);
    }
    pub fn write_u16(&mut self, val: u16) {
        self.write_bytes(&val.to_le_bytes()[..]);
    }
    pub fn write_u32(&mut self, val: u32) {
        self.write_bytes(&val.to_le_bytes()[..]);
    }
    pub fn write_u64(&mut self, val: u64) {
        self.write_bytes(&val.to_le_bytes()[..]);
    }
    pub fn write_bytes(&mut self, data: &[u8]) {
        let copy_len = data.len();
        let data_len = self.len();
        let wr_off = self.write_offset;
        assert!(copy_len <= data_len.checked_sub(wr_off).unwrap());

        match &mut self.inner {
            ROInner::Buf(b) => {
                b[wr_off..(wr_off + copy_len)]
                    .copy_from_slice(&data[..copy_len]);
            }
            ROInner::Map(m) => {
                // FIXME: this does not properly apply write offsetting
                m.write_bytes(data).unwrap();
            }
        }
        self.write_offset += copy_len;
    }
    pub fn fill(&mut self, val: u8) {
        match &mut self.inner {
            ROInner::Buf(buf) => {
                for b in buf[self.write_offset..].iter_mut() {
                    *b = val
                }
            }
            ROInner::Map(m) => {
                m.write_byte(val, m.len() - self.write_offset).unwrap();
            }
        }
        self.write_offset = self.len();
    }
}

enum WOInner<'a> {
    Buf(&'a [u8]),
    Map(SubMapping<'a>),
}

/// Represents an abstract requested write operation.
///
/// Exposes an API with various "read" methods, which fulfill the request.
pub struct WriteOp<'a> {
    inner: WOInner<'a>,
    offset: usize,
    read_offset: usize,
}
impl<'a> WriteOp<'a> {
    /// Initializes a new write operation from a mapping.
    ///
    /// # Arguments
    ///
    /// - `op_offset`: An auxiliary offset stored within the operation,
    /// identifying the region within the emulated resource where `mapping` should
    /// be stored.
    /// - `mapping`: A mapping which represents the "source" of the write operation.
    pub fn from_mapping(op_offset: usize, mapping: SubMapping<'a>) -> Self {
        Self { inner: WOInner::Map(mapping), offset: op_offset, read_offset: 0 }
    }

    /// Initializes a new write operation from a buffer.
    ///
    /// # Arguments
    ///
    /// - `op_offset`: An auxiliary offset stored within the operation,
    /// identifying the region within the emulated resource where `buf` should
    /// be stored.
    /// - `buf`: A buffer which represents the "source" of the write operation.
    pub fn from_buf(op_offset: usize, buf: &'a [u8]) -> Self {
        Self { inner: WOInner::Buf(buf), offset: op_offset, read_offset: 0 }
    }

    /// Constructs a child write operation from within an existing write
    /// operation.
    ///
    /// # Arguments
    ///
    /// - `op_offset`: Offset of the child operation. Does not need to correlate
    /// to the `parent` operation's offset.
    /// - `parent`: The operation from which this operation is being split.
    /// - `range`: The location within the parent operation to be moved
    /// to the child.
    pub fn new_child<'b, R>(
        op_offset: usize,
        parent: &'a mut WriteOp,
        range: R,
    ) -> WriteOp<'b>
    where
        'a: 'b,
        R: RangeBounds<usize> + SliceIndex<[u8], Output = [u8]>,
    {
        match &mut parent.inner {
            WOInner::Buf(b) => WriteOp {
                inner: WOInner::Buf(&b[range]),
                offset: op_offset,
                read_offset: 0,
            },
            WOInner::Map(m) => {
                let (start, end) = numeric_bounds(range, m.len());
                let len = end - start;
                let m = m.subregion(start, len).unwrap();
                WriteOp {
                    inner: WOInner::Map(m),
                    offset: op_offset,
                    read_offset: 0,
                }
            }
        }
    }

    pub fn len(&self) -> usize {
        match &self.inner {
            WOInner::Buf(b) => b.len(),
            WOInner::Map(m) => m.len(),
        }
    }
    pub fn avail(&self) -> usize {
        self.len().checked_sub(self.read_offset).unwrap()
    }
    pub fn offset(&self) -> usize {
        self.offset
    }

    fn read_val<const COUNT: usize>(&mut self) -> [u8; COUNT] {
        let mut buf = [0u8; COUNT];
        self.read_bytes(&mut buf);
        buf
    }
    pub fn read_u8(&mut self) -> u8 {
        u8::from_le_bytes(self.read_val())
    }
    pub fn read_u16(&mut self) -> u16 {
        u16::from_le_bytes(self.read_val())
    }
    pub fn read_u32(&mut self) -> u32 {
        u32::from_le_bytes(self.read_val())
    }
    pub fn read_u64(&mut self) -> u64 {
        u64::from_le_bytes(self.read_val())
    }
    pub fn read_bytes(&mut self, data: &mut [u8]) {
        let copy_len = data.len();
        if copy_len == 0 {
            return;
        }
        let data_len = self.len();
        let rd_off = self.read_offset;
        assert!(copy_len <= data_len.checked_sub(rd_off).unwrap());
        match &mut self.inner {
            WOInner::Buf(b) => {
                data[..copy_len]
                    .copy_from_slice(&b[rd_off..(rd_off + copy_len)]);
            }
            WOInner::Map(m) => {
                // FIXME: this does not properly apply read offsetting
                m.read_bytes(data).unwrap();
            }
        }
        self.read_offset += copy_len;
    }
}

pub enum RWOp<'a, 'b> {
    Read(&'a mut ReadOp<'b>),
    Write(&'a mut WriteOp<'b>),
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
            RWOp::Read(ro) => ro.len(),
            RWOp::Write(wo) => wo.len(),
        }
    }
    pub fn is_read(&self) -> bool {
        matches!(self, RWOp::Read(_))
    }
    pub fn is_write(&self) -> bool {
        matches!(self, RWOp::Write(_))
    }
}

/// An address within a guest VM.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct GuestAddr(pub u64);

impl GuestAddr {
    pub fn offset<T: Sized>(&self, count: usize) -> Self {
        Self(self.0 + (count * std::mem::size_of::<T>()) as u64)
    }
}

/// A region of memory within a guest VM.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
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

pub use crate::lifecycle::Lifecycle;

pub const PAGE_SIZE: usize = 0x1000;
pub const PAGE_OFFSET: usize = 0xfff;
pub const PAGE_MASK: usize = usize::MAX - PAGE_OFFSET;
pub const PAGE_SHIFT: usize = 12;

pub fn round_up_p2(val: usize, to: usize) -> usize {
    assert!(to.is_power_of_two());
    assert!(to != 0);

    val.checked_add(to - 1).unwrap() & !(to - 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn readop_base_size() {
        let mut buf = [0u8; 8];
        let mut ro8 = ReadOp::from_buf(0, &mut buf[0..1]);
        ro8.write_u8(1);
        drop(ro8);
        assert_eq!(buf, [1, 0, 0, 0, 0, 0, 0, 0]);

        let mut ro16 = ReadOp::from_buf(0, &mut buf[0..2]);
        ro16.write_u16(0x2000);
        drop(ro16);
        assert_eq!(buf, [0, 0x20, 0, 0, 0, 0, 0, 0]);

        let mut ro32 = ReadOp::from_buf(0, &mut buf[0..4]);
        ro32.write_u32(0x4000_0000);
        drop(ro32);
        assert_eq!(buf, [0, 0, 0, 0x40, 0, 0, 0, 0]);

        let mut ro64 = ReadOp::from_buf(0, &mut buf);
        ro64.write_u64(0x8000_0000_0000_0000);
        drop(ro64);
        assert_eq!(buf, [0, 0, 0, 0, 0, 0, 0, 0x80]);
    }

    #[test]
    fn writeop_base_size() {
        let buf = [0x10, 0x20, 0x30, 0x40, 0x50, 0x60, 0x70, 0x80];
        let mut wo8 = WriteOp::from_buf(0, &buf[0..1]);
        assert_eq!(wo8.read_u8(), 0x10);
        drop(wo8);

        let mut wo16 = WriteOp::from_buf(0, &buf[0..2]);
        assert_eq!(wo16.read_u16(), 0x2010);
        drop(wo16);

        let mut wo32 = WriteOp::from_buf(0, &buf[0..4]);
        assert_eq!(wo32.read_u32(), 0x40302010);
        drop(wo32);

        let mut wo64 = WriteOp::from_buf(0, &buf);
        assert_eq!(wo64.read_u64(), 0x8070605040302010);
        drop(wo64);
    }

    #[test]
    #[should_panic]
    fn readop_oversize() {
        let mut buf = [0u8];
        let mut ro8 = ReadOp::from_buf(0, &mut buf);
        ro8.write_u16(0x1000);
    }

    #[test]
    #[should_panic]
    fn writeop_oversize() {
        let buf = [0u8];
        let mut wo8 = WriteOp::from_buf(0, &buf);
        let _ = wo8.read_u16();
    }

    #[test]
    fn readop_short() {
        let mut buf = [0u8, 0u8];
        let mut ro = ReadOp::from_buf(0, &mut buf);
        ro.write_u8(0x10);
        ro.write_u8(0x20);
        drop(ro);
        assert_eq!(buf, [0x10, 0x20]);
    }

    #[test]
    fn writeop_short() {
        let buf = [0x10, 0x20];
        let mut wo = WriteOp::from_buf(0, &buf);
        assert_eq!(wo.read_u8(), 0x10);
        assert_eq!(wo.read_u8(), 0x20);
    }
}
