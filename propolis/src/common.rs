use std::ops::{Add, BitAnd};
use std::ops::{Bound::*, RangeBounds};
use std::ptr::{copy_nonoverlapping, write_bytes};
use std::slice::SliceIndex;

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
    Ptr(*mut u8, usize),
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
    /// Initializes a new read operation from a buffer.
    ///
    /// # Arguments
    ///
    /// - `op_offset`: An auxiliary offset stored within the operation,
    /// identifying the region which should be accessed to populate `buf`.
    /// - `buf`: A buffer which represents the "sink" of the read operation.
    pub fn new_buf(op_offset: usize, buf: &'a mut [u8]) -> Self {
        Self { inner: ROInner::Buf(buf), offset: op_offset, write_offset: 0 }
    }
    /// Safety: The provided `ptr` must be valid and writable for `len` bytes
    /// and must not alias to any of the inputs to write_* calls on this ReadOp.
    pub unsafe fn new_ptr(op_offset: usize, ptr: *mut u8, len: usize) -> Self {
        Self {
            inner: ROInner::Ptr(ptr, len),
            offset: op_offset,
            write_offset: 0,
        }
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
            ROInner::Ptr(p, len) => {
                let (start, end) = numeric_bounds(range, *len);
                let pnew = unsafe { p.add(start) };
                ReadOp {
                    inner: ROInner::Ptr(pnew, end - start),
                    offset: op_offset,
                    write_offset: 0,
                }
            }
        }
    }

    pub fn len(&self) -> usize {
        match &self.inner {
            ROInner::Buf(b) => b.len(),
            ROInner::Ptr(_p, l) => *l,
        }
    }
    pub fn avail(&self) -> usize {
        self.len().checked_sub(self.write_offset).unwrap()
    }
    pub fn offset(&self) -> usize {
        self.offset
    }

    fn write_val<T: Sized>(&mut self, val: T) {
        // Safety: Taking legal a reference to `val` here means that no internal
        // pointer(s) should overlap with it.
        unsafe {
            self.write_raw(
                &val as *const T as *const u8,
                ::std::mem::size_of::<T>(),
            );
        }
    }
    pub fn write_u8(&mut self, val: u8) {
        self.write_val(val);
    }
    pub fn write_u16(&mut self, val: u16) {
        self.write_val(val);
    }
    pub fn write_u32(&mut self, val: u32) {
        self.write_val(val);
    }
    pub fn write_u64(&mut self, val: u64) {
        self.write_val(val);
    }
    pub fn write_bytes(&mut self, data: &[u8]) {
        // Safety: With a legal reference to `data`, no internal pointer(s)
        // should overlap with it.
        unsafe {
            self.write_raw(data.as_ptr(), data.len());
        }
    }
    pub fn fill(&mut self, val: u8) {
        let wr_off = self.write_offset;
        match &mut self.inner {
            ROInner::Buf(buf) => {
                for b in buf[wr_off..].iter_mut() {
                    *b = val
                }
            }
            ROInner::Ptr(p, l) => unsafe {
                write_bytes(p.add(wr_off), val, l.checked_sub(wr_off).unwrap());
            },
        }
        self.write_offset = self.len();
    }

    /// Safety: The `copy_len` bytes of memory, pointed to by `data` must not
    /// overlap with any of the memory contained by this ReadOp
    pub unsafe fn write_raw(&mut self, data: *const u8, copy_len: usize) {
        if copy_len == 0 {
            return;
        }
        let data_len = self.len();
        let wr_off = self.write_offset;
        assert!(copy_len <= data_len.checked_sub(wr_off).unwrap());

        match &mut self.inner {
            ROInner::Buf(b) => {
                copy_nonoverlapping(data, b[wr_off..].as_mut_ptr(), copy_len);
            }
            ROInner::Ptr(p, _l) => {
                copy_nonoverlapping(data, p.add(wr_off), copy_len);
            }
        }
        self.write_offset += copy_len;
    }
}

enum WOInner<'a> {
    Buf(&'a [u8]),
    Ptr(*const u8, usize),
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
    /// Initializes a new write operation from a buffer.
    ///
    /// # Arguments
    ///
    /// - `op_offset`: An auxiliary offset stored within the operation,
    /// identifying the region within the emulated resource where `buf` should
    /// be stored.
    /// - `buf`: A buffer which represents the "source" of the write operation.
    pub fn new_buf(op_offset: usize, buf: &'a [u8]) -> Self {
        Self { inner: WOInner::Buf(buf), offset: op_offset, read_offset: 0 }
    }
    /// Safety: The provided `ptr` must be valid and readable for `len` bytes
    /// and must not alias to any of the inputs to read_* calls on this WriteOp.
    pub unsafe fn new_ptr(
        op_offset: usize,
        ptr: *const u8,
        len: usize,
    ) -> Self {
        Self {
            inner: WOInner::Ptr(ptr, len),
            offset: op_offset,
            read_offset: 0,
        }
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
            WOInner::Ptr(p, len) => {
                let (start, end) = numeric_bounds(range, *len);
                let pnew = unsafe { p.add(start) };
                WriteOp {
                    inner: WOInner::Ptr(pnew, end - start),
                    offset: op_offset,
                    read_offset: 0,
                }
            }
        }
    }

    pub fn len(&self) -> usize {
        match &self.inner {
            WOInner::Buf(b) => b.len(),
            WOInner::Ptr(_p, l) => *l,
        }
    }
    pub fn avail(&self) -> usize {
        self.len().checked_sub(self.read_offset).unwrap()
    }
    pub fn offset(&self) -> usize {
        self.offset
    }

    fn read_val<T: Sized + Copy + Default>(&mut self) -> T {
        let mut val: T = Default::default();
        // Safety: Since `val` is created and referenced locally, it should not
        // overlap with any inner pointer.
        unsafe {
            self.read_raw(
                &mut val as *mut T as *mut u8,
                ::std::mem::size_of::<T>(),
            );
        }
        val
    }
    pub fn read_u8(&mut self) -> u8 {
        self.read_val()
    }
    pub fn read_u16(&mut self) -> u16 {
        self.read_val()
    }
    pub fn read_u32(&mut self) -> u32 {
        self.read_val()
    }
    pub fn read_u64(&mut self) -> u64 {
        self.read_val()
    }
    pub fn read_bytes(&mut self, buf: &mut [u8]) {
        // Safety: Since `buf` is a legal mutable reference, it should not
        // overlap with any inner pointer.
        unsafe {
            self.read_raw(buf.as_mut_ptr(), buf.len());
        }
    }

    /// Safety: The `copy_len` bytes of memory, pointed to by `data` must not
    /// overlap with any of the memory contained by this WriteOp.
    pub unsafe fn read_raw(&mut self, data: *mut u8, copy_len: usize) {
        if copy_len == 0 {
            return;
        }
        let data_len = self.len();
        let rd_off = self.read_offset;
        assert!(copy_len <= data_len.checked_sub(rd_off).unwrap());

        match &mut self.inner {
            WOInner::Buf(b) => {
                copy_nonoverlapping(b[rd_off..].as_ptr(), data, copy_len);
            }
            WOInner::Ptr(p, _l) => {
                copy_nonoverlapping(p.add(rd_off), data, copy_len);
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
#[derive(Copy, Clone, Debug)]
pub struct GuestAddr(pub u64);

/// A region of memory within a guest VM.
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

pub use crate::inventory::Entity;

pub const PAGE_SIZE: usize = 0x1000;
pub const PAGE_OFFSET: usize = 0xfff;
pub const PAGE_MASK: usize = usize::MAX - PAGE_OFFSET;
pub const PAGE_SHIFT: usize = 12;

pub fn round_up_p2(val: usize, to: usize) -> usize {
    assert!(to.is_power_of_two());
    assert!(to != 0);

    val.checked_add(to - 1).unwrap() & !(to - 1)
}
