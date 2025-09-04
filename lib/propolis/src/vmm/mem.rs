// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Module for managing guest memory mappings.

use std::io::{Error, ErrorKind, Result};
use std::marker::PhantomData;
use std::mem::{size_of, size_of_val, MaybeUninit};
use std::ops::RangeInclusive;
use std::os::unix::io::{AsRawFd, RawFd};
use std::ptr::{copy_nonoverlapping, NonNull};
use std::sync::Arc;

use libc::iovec;

use crate::accessors::MemAccessor;
use crate::common::{
    GuestAddr, GuestData, GuestRegion, PAGE_MASK, PAGE_SHIFT, PAGE_SIZE,
};
use crate::util::aspace::ASpace;
use crate::vmm::VmmHdl;

use zerocopy::FromBytes;

bitflags! {
    /// Bitflags representing memory protections.
    #[derive(Debug, Copy, Clone)]
    pub struct Prot: u8 {
        const NONE = 0;
        const READ = libc::PROT_READ as u8;
        const WRITE = libc::PROT_WRITE as u8;
        const EXEC = libc::PROT_EXEC as u8;
        const RW = (libc::PROT_READ
                    | libc::PROT_WRITE) as u8;
        const ALL = (libc::PROT_READ
                    | libc::PROT_WRITE
                    | libc::PROT_EXEC) as u8;
    }
}

pub(crate) struct MapSeg {
    id: i32,

    /// Mapping of the guest (physical) address space, subject to the protection
    /// restrictions enforced on the guest.
    map_guest: Arc<Mapping>,

    /// Mapping of the memory segment object itself, with full (R/W) access to
    /// its contents.
    map_seg: Arc<Mapping>,
}

pub(crate) enum MapKind {
    Dram(MapSeg),
    Rom(MapSeg),
    MmioReserve,
}

pub(crate) struct MapEnt {
    name: String,
    kind: MapKind,
}

impl MapEnt {
    fn map_type(&self) -> MapType {
        match &self.kind {
            MapKind::Dram(_) => MapType::Dram,
            MapKind::Rom(_) => MapType::Rom,
            MapKind::MmioReserve => MapType::Mmio,
        }
    }
}

pub enum MapType {
    Dram,
    Rom,
    Mmio,
}

pub struct PhysMap {
    map: Arc<ASpace<MapEnt>>,
    hdl: Arc<VmmHdl>,
    next_segid: i32,
}
impl PhysMap {
    pub(crate) fn new(size: usize, hdl: Arc<VmmHdl>) -> Self {
        assert!(size != 0);
        assert!(size & PAGE_SIZE == 0, "size must be page-aligned");

        Self { map: Arc::new(ASpace::new(0, size - 1)), hdl, next_segid: 0 }
    }

    pub(crate) fn map_mut(&mut self) -> &mut ASpace<MapEnt> {
        Arc::get_mut(&mut self.map).expect(
            "map should not be accessed mutably after PhysMap finalization",
        )
    }

    /// Create and map a memory region for the guest
    pub(crate) fn add_mem(
        &mut self,
        name: String,
        addr: usize,
        size: usize,
    ) -> Result<()> {
        let (segid, map_guest, map_seg) =
            self.seg_create_map(addr, size, None)?;

        self.map_mut()
            .register(
                addr,
                size,
                MapEnt {
                    name,
                    kind: MapKind::Dram(MapSeg {
                        id: segid,
                        map_guest,
                        map_seg,
                    }),
                },
            )
            .map_err(Error::from)
    }

    /// Create and map a ROM region for the guest
    pub(crate) fn add_rom(
        &mut self,
        name: String,
        addr: usize,
        size: usize,
    ) -> Result<()> {
        let (segid, map_guest, map_seg) =
            self.seg_create_map(addr, size, Some(&name))?;

        self.map_mut()
            .register(
                addr,
                size,
                MapEnt {
                    name,
                    kind: MapKind::Rom(MapSeg {
                        id: segid,
                        map_guest,
                        map_seg,
                    }),
                },
            )
            .map_err(Error::from)
    }

    /// Mark a region of the guest address space as reserved for MMIO
    pub(crate) fn add_mmio_reservation(
        &mut self,
        name: String,
        addr: usize,
        size: usize,
    ) -> Result<()> {
        self.map_mut()
            .register(addr, size, MapEnt { name, kind: MapKind::MmioReserve })
            .map_err(Error::from)
    }

    pub(crate) fn post_reinit(&self) -> Result<()> {
        // Since VM_REINIT unmaps all non-sysmem segments from the address space
        // of the VM, we must reestablish the ROM mapping(s) now.
        for (addr, len, ent) in self.map.iter() {
            if let MapKind::Rom(detail) = &ent.kind {
                self.hdl.map_memseg(
                    detail.id,
                    addr,
                    len,
                    0,
                    Prot::READ | Prot::EXEC,
                )?;
            }
        }
        Ok(())
    }

    pub fn mappings(&self) -> Vec<(usize, usize, MapType)> {
        self.map
            .iter()
            .map(|(addr, len, ent)| (addr, len, ent.map_type()))
            .collect()
    }

    pub(crate) fn finalize(&mut self) -> MemAccessor {
        assert!(
            Arc::strong_count(&self.map) == 1,
            "finalize should only be called once"
        );
        MemAccessor::new(Arc::new(MemCtx { map: self.map.clone() }))
    }

    /// Allocate a backing memseg, map it into the guest-physical space, and map
    /// both (the segment and guest mapping) into the process-virtual space.
    fn seg_create_map(
        &mut self,
        addr: usize,
        size: usize,
        rom_name: Option<&str>,
    ) -> Result<(i32, Arc<Mapping>, Arc<Mapping>)> {
        let prot = match rom_name.as_ref() {
            Some(_) => Prot::READ | Prot::EXEC,
            None => Prot::ALL,
        };

        let segid = self.next_segid;
        self.hdl.create_memseg(segid, size, rom_name)?;
        self.hdl.map_memseg(segid, addr, size, 0, prot)?;
        self.next_segid += 1;
        // TODO: if we somehow fail the later stages of this operation, the
        // memseg and its mapping established in the VMM will persist

        let seg_off = self.hdl.devmem_offset(segid)?;
        let map_guest = Mapping::new(size, prot, &self.hdl, addr as i64)?;
        let map_seg = Mapping::new(size, Prot::RW, &self.hdl, seg_off as i64)?;

        Ok((segid, map_guest, map_seg))
    }

    pub(crate) fn destroy(&mut self) {
        let map = Arc::get_mut(&mut self.map).expect(
            "no refs should remain to Physmap contents when destroy() called",
        );
        map.clear();
    }
}

#[cfg(test)]
impl PhysMap {
    pub(crate) fn new_test(size: usize) -> Self {
        let hdl =
            VmmHdl::new_test(size).expect("create tempfile backed test hdl");
        Self::new(size, Arc::new(hdl))
    }

    /// Create "memory" region on an instance backed with a fake VmmHdl
    pub(crate) fn add_test_mem(
        &mut self,
        name: String,
        addr: usize,
        size: usize,
    ) -> Result<()> {
        let (map_guest, map_seg) = self.seg_test_map(addr, size, false)?;
        self.map_mut()
            .register(
                addr,
                size,
                MapEnt {
                    name,
                    kind: MapKind::Dram(MapSeg { id: -1, map_guest, map_seg }),
                },
            )
            .map_err(Error::from)
    }

    /// Create "ROM" region on an instance backed with a fake VmmHdl
    pub(crate) fn add_test_rom(
        &mut self,
        name: String,
        addr: usize,
        size: usize,
    ) -> Result<()> {
        let (map_guest, map_seg) = self.seg_test_map(addr, size, true)?;
        self.map_mut()
            .register(
                addr,
                size,
                MapEnt {
                    name,
                    kind: MapKind::Rom(MapSeg { id: -1, map_guest, map_seg }),
                },
            )
            .map_err(Error::from)
    }

    /// Make fake VmmHdl (backed with tempfile) for use in testing
    fn seg_test_map(
        &mut self,
        addr: usize,
        size: usize,
        is_rom: bool,
    ) -> Result<(Arc<Mapping>, Arc<Mapping>)> {
        let prot = match is_rom {
            true => Prot::READ,
            false => Prot::RW,
        };
        let map_guest = Mapping::new(size, prot, &self.hdl, addr as i64)?;
        let map_seg = Mapping::new(size, Prot::RW, &self.hdl, addr as i64)?;

        Ok((map_guest, map_seg))
    }
}

// TODO: reword?
/// A owned region of mapped guest memory, accessible via [`SubMapping`].
///
/// When dealing with raw pointers, caution must be taken to dereference the
/// pointer safely:
/// - The pointer must not be null
/// - The dereferenced pointer must be within bounds of a valid mapping
///
/// Additionally, aliasing rules apply to references:
/// - References cannot outlive their referents
/// - Mutable references cannot be aliased
///
/// These issues become especially hairy across mappings, where an
/// out-of-process entity (i.e., the guest, hardware, etc) may modify memory.
///
/// This structure provides an interface which upholds the following conditions:
/// - Reads to a memory region are only permitted if the mapping is readable.
/// - Writes to a memory region are only permitted if the mapping is writable.
/// - References to memory are not exposed from the structure.

#[derive(Debug)]
pub(crate) struct Mapping {
    ptr: NonNull<u8>,
    len: usize,
    prot: Prot,
}
impl Mapping {
    /// Creates a new memory mapping from a `VmmHdl`, with the requested
    /// permissions.  The `size` and `devoff` must be `PAGE_SIZE` aligned.
    fn new(
        size: usize,
        prot: Prot,
        vmm: &VmmHdl,
        devoff: i64,
    ) -> Result<Arc<Self>> {
        let fd = vmm.fd();

        // We do not mmap() guest resources with the EXEC bit set
        let mmap_prot = prot.intersection(Prot::RW);

        // Safety:
        // With a NULL `addr, the OS will pick a mapping location which does not
        // conflict with other resources.  While the VmmFile is not something
        // that should be truncated, it is the responsibility of the caller to
        // ensure that the underlying resources are not destroyed prior to
        // `Mapping`s which refer to them.
        let ptr = unsafe {
            libc::mmap(
                core::ptr::null_mut(),
                size,
                mmap_prot.bits().into(),
                libc::MAP_SHARED,
                fd,
                devoff,
            )
        };

        if ptr == libc::MAP_FAILED {
            return Err(Error::last_os_error());
        }
        let ptr = NonNull::new(ptr as *mut u8)
            .expect("mmap() result should be non-NULL");

        Ok(Arc::new(Self { ptr, len: size, prot }))
    }
}
impl Drop for Mapping {
    fn drop(&mut self) {
        unsafe {
            libc::munmap(self.ptr.as_ptr() as *mut libc::c_void, self.len);
        }
    }
}

// Safety: `Mapping`'s API does not provide raw access to the underlying
// pointer, nor any mechanism to create references to the underlying data.
unsafe impl Send for Mapping {}
unsafe impl Sync for Mapping {}

#[derive(Debug)]
// Backing resources (Mapping or SubMapping) must remain held, even though we do
// not reference them directly as a field.
#[allow(dead_code)]
enum Backing<'a> {
    Base(Arc<Mapping>),
    Sub(&'a SubMapping<'a>),
}

/// A borrowed region from a `Mapping` object.
///
/// Provides interfaces for acting on memory, but does not own the
/// underlying memory region.
#[derive(Debug)]
pub struct SubMapping<'a> {
    // The backing resource must remain held, even though we never reference it
    // directly as a field.
    #[allow(unused)]
    backing: Backing<'a>,

    ptr: NonNull<u8>,
    len: usize,
    prot: Prot,
}

impl SubMapping<'_> {
    /// Create `SubMapping` using the entire region offered by an underlying
    /// `Mapping` object.
    fn new_base<'a>(
        _mem: &'a MemCtx,
        base: &'_ Arc<Mapping>,
    ) -> SubMapping<'a> {
        SubMapping {
            backing: Backing::Base(base.clone()),

            ptr: base.ptr,
            len: base.len,
            prot: base.prot,
        }
    }

    /// Create `SubMapping` using entire region offered by existing `SubMapping`
    /// object.
    fn new_sub(&self) -> SubMapping<'_> {
        SubMapping {
            backing: Backing::Sub(self),

            ptr: self.ptr,
            len: self.len,
            prot: self.prot,
        }
    }

    #[cfg(test)]
    fn new_base_test<'a>(base: Arc<Mapping>) -> SubMapping<'a> {
        let ptr = base.ptr;
        let len = base.len;
        let prot = base.prot;
        SubMapping { backing: Backing::Base(base), ptr, len, prot }
    }

    /// Acquire a reference to a region of memory within the
    /// current mapping.
    ///
    /// - `offset` is relative to the current mapping.
    /// - `length` is the length of the new subregion.
    ///
    /// Returns `None` if the requested offset/length extends beyond the end of
    /// the mapping.
    pub fn subregion(
        &self,
        offset: usize,
        length: usize,
    ) -> Option<SubMapping<'_>> {
        self.new_sub().constrain_region(offset, length).ok()
    }

    /// Constrain the access permissions of a SubMapping
    pub fn constrain_access(mut self, prot_limit: Prot) -> Self {
        self.prot = self.prot.intersection(prot_limit);
        self
    }

    /// Attempt to constrain the `SubMapping` to a subset of the memory which it
    /// currently covers.
    ///
    /// - `offset` is relative to the current mapping.
    /// - `length` is the length of the new subregion.
    ///
    /// Returns the constrained `SubMapping` when successful, or the unchanged
    /// `SubMapping` if invalid input resulted in an error.
    pub fn constrain_region(
        mut self,
        offset: usize,
        length: usize,
    ) -> std::result::Result<Self, Self> {
        let end = match offset.checked_add(length) {
            Some(v) => v,
            None => return Err(self),
        };
        if self.len < end {
            return Err(self);
        }

        // Safety:
        // - Starting and resulting pointer must be within bounds or
        // one past the end of the same allocated object.
        // - The computed offset, in bytes, cannot overflow isize.
        // - The offset cannot rely on "wrapping around" the address
        // space.
        let ptr = unsafe { self.ptr.as_ptr().add(offset) };
        self.ptr = NonNull::new(ptr).unwrap();
        self.len = length;
        Ok(self)
    }

    /// Emit appropriate error if mapping does not allow writes
    fn check_write_access(&self) -> Result<()> {
        if !self.prot.contains(Prot::WRITE) {
            Err(Error::new(ErrorKind::PermissionDenied, "No write access"))
        } else {
            Ok(())
        }
    }

    /// Emit appropriate error if mapping does not allow reads
    fn check_read_access(&self) -> Result<()> {
        if !self.prot.contains(Prot::READ) {
            Err(Error::new(ErrorKind::PermissionDenied, "No read access"))
        } else {
            Ok(())
        }
    }

    /// Reads a `T` object from the mapping.
    pub fn read<T: Copy + FromBytes>(&self) -> Result<T> {
        self.check_read_access()?;
        let typed = self.ptr.as_ptr() as *const T;
        if self.len < std::mem::size_of::<T>() {
            return Err(Error::new(ErrorKind::InvalidData, "Buffer too small"));
        }

        // Safety:
        // - typed must be valid for reads: `check_read_access()` succeeded
        // - typed must point to a properly initialized value of T: always true
        //     because we require `T: FromBytes`. `zerocopy::FromBytes` happens
        //     to have the same concerns as us - that T is valid for all bit
        //     patterns.
        Ok(unsafe { typed.read_unaligned() })
    }

    /// Read `values` from the mapping.
    pub fn read_many<T: Copy + FromBytes>(
        &self,
        values: &mut [T],
    ) -> Result<()> {
        self.check_read_access()?;
        let copy_len = size_of_val(values);
        if self.len < copy_len {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "Value larger than mapping",
            ));
        }

        // We know that the `values` reference is properly aligned, but that is
        // not guaranteed for the source pointer.  Cast it down to a u8, which
        // will appease those alignment concerns
        let src = self.ptr.as_ptr() as *const u8;
        // We know reinterpreting `*mut T` as `*mut u8` and writing to it cannot
        // result in invalid `T` because `T: FromBytes`
        let dst = values.as_mut_ptr() as *mut u8;

        // Safety
        // - `src` is valid for read for the `copy_len` as checked above
        // - `dst` is valid for writes for its entire length, since it is from a
        // valid mutable reference passed in to us
        // - both are aligned for a `u8` copy
        // - `dst` cannot be overlapped by `src`, since the former came from a
        //   valid reference, and references to guest mappings are not allowed
        unsafe {
            copy_nonoverlapping(src, dst, copy_len);
        }
        Ok(())
    }

    /// Reads a buffer of bytes from the mapping.
    ///
    /// If `buf` is larger than the SubMapping, the read will be truncated to
    /// length of the SubMapping.
    ///
    /// Returns the number of bytes read.
    pub fn read_bytes(&self, buf: &mut [u8]) -> Result<usize> {
        let read_len = usize::min(buf.len(), self.len);
        self.read_many(&mut buf[..read_len])?;
        Ok(read_len)
    }

    /// Reads a buffer of bytes from the mapping into an uninitialized region
    ///
    /// If `buf` is larger than the SubMapping, the read will be truncated to
    /// length of the SubMapping.
    ///
    /// Returns the number of bytes read.
    pub fn read_bytes_uninit(
        &self,
        buf: &mut [MaybeUninit<u8>],
    ) -> Result<usize> {
        let read_len = usize::min(buf.len(), self.len);
        self.read_many(&mut buf[..read_len])?;
        Ok(read_len)
    }

    /// Pread from `file` into the mapping.
    pub fn pread(
        &self,
        file: &impl AsRawFd,
        length: usize,
        offset: i64,
    ) -> Result<usize> {
        self.check_write_access()?;
        let to_read = usize::min(length, self.len);
        let read = unsafe {
            libc::pread(
                file.as_raw_fd(),
                self.ptr.as_ptr() as *mut libc::c_void,
                to_read,
                offset,
            )
        };
        if read == -1 {
            return Err(Error::last_os_error());
        }
        Ok(read as usize)
    }

    /// Writes `value` into the mapping.
    pub fn write<T: Copy>(&self, value: &T) -> Result<()> {
        self.check_write_access()?;
        let typed = self.ptr.as_ptr() as *mut T;
        unsafe {
            typed.write_unaligned(*value);
        }
        Ok(())
    }

    /// Writes `values` into the mapping.
    pub fn write_many<T: Copy>(&self, values: &[T]) -> Result<()> {
        self.check_write_access()?;
        let copy_len = size_of_val(values);
        if self.len < copy_len {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "Value larger than mapping",
            ));
        }

        // We know that the `values` reference is properly aligned, but that is
        // not guaranteed for the destination pointer.  Cast it down to a u8,
        // which will appease those alignment concerns
        let src = values.as_ptr() as *const u8;
        let dst = self.ptr.as_ptr();

        // Safety
        // - `src` is valid for reads for its entire length, since it is from a
        //   valid reference passed in to us
        // - `dst` is valid for writes for the `copy_len` as checked above
        // - both are aligned for a `u8` copy
        // - `dst` cannot be overlapped by `src`, since the latter came from a
        //   valid reference, and references to guest mappings are not allowed
        unsafe {
            copy_nonoverlapping(src, dst, copy_len);
        }
        Ok(())
    }

    /// Writes a buffer of bytes into the mapping.
    ///
    /// If `buf` is larger than the SubMapping, the write will be truncated to
    /// length of the SubMapping.
    ///
    /// Returns the number of bytes written.
    pub fn write_bytes(&self, buf: &[u8]) -> Result<usize> {
        let write_len = usize::min(buf.len(), self.len);
        self.write_many(&buf[..write_len])?;
        Ok(write_len)
    }

    /// Writes a single byte `val` to the mapping, `count` times.
    pub fn write_byte(&self, val: u8, count: usize) -> Result<usize> {
        self.check_write_access()?;
        let to_copy = usize::min(count, self.len);
        unsafe {
            self.ptr.as_ptr().write_bytes(val, to_copy);
        }
        Ok(to_copy)
    }

    /// Pwrite from the mapping to `file`.
    pub fn pwrite(
        &self,
        file: &impl AsRawFd,
        length: usize,
        offset: i64,
    ) -> Result<usize> {
        self.check_read_access()?;
        let to_write = usize::min(length, self.len);
        let written = unsafe {
            libc::pwrite(
                file.as_raw_fd(),
                self.ptr.as_ptr() as *const libc::c_void,
                to_write,
                offset,
            )
        };
        if written == -1 {
            return Err(Error::last_os_error());
        }
        Ok(written as usize)
    }

    /// Returns the length of the mapping.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Returns true if the mapping is empty.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn prot(&self) -> Prot {
        self.prot
    }

    /// Returns a raw readable reference to the underlying data.
    ///
    /// # Safety
    ///
    /// - The caller must never create a reference to the underlying
    /// memory region.
    /// - The returned pointer must not outlive the mapping.
    /// - The caller may only read up to `len()` bytes.
    pub unsafe fn raw_readable(&self) -> Option<*const u8> {
        if self.prot.contains(Prot::READ) {
            Some(self.ptr.as_ptr() as *const u8)
        } else {
            None
        }
    }

    /// Returns a raw writable reference to the underlying data.
    ///
    /// # Safety
    ///
    /// - The caller must never create a reference to the underlying
    /// memory region.
    /// - The returned pointer must not outlive the mapping.
    /// - The caller may only write up to `len()` bytes.
    pub unsafe fn raw_writable(&self) -> Option<*mut u8> {
        if self.prot.contains(Prot::WRITE) {
            Some(self.ptr.as_ptr())
        } else {
            None
        }
    }
}

// Safety: `SubMapping`'s API does not provide raw access to the underlying
// pointer, nor any mechanism to create references to the underlying data.
unsafe impl Send for SubMapping<'_> {}
unsafe impl Sync for SubMapping<'_> {}

pub trait MappingExt {
    /// preadv from `file` into multiple mappings
    fn preadv(&self, fd: RawFd, offset: i64) -> Result<usize>;

    /// pwritev from multiple mappings to `file`
    fn pwritev(&self, fd: RawFd, offset: i64) -> Result<usize>;
}

impl<'a, T: AsRef<[SubMapping<'a>]>> MappingExt for T {
    fn preadv(&self, fd: RawFd, offset: i64) -> Result<usize> {
        if !self
            .as_ref()
            .iter()
            .all(|mapping| mapping.prot.contains(Prot::WRITE))
        {
            return Err(Error::new(
                ErrorKind::PermissionDenied,
                "No write access",
            ));
        }

        let iov = self
            .as_ref()
            .iter()
            .map(|mapping| iovec {
                iov_base: mapping.ptr.as_ptr() as *mut libc::c_void,
                iov_len: mapping.len,
            })
            .collect::<Vec<_>>();

        let read = unsafe {
            libc::preadv(fd, iov.as_ptr(), iov.len() as libc::c_int, offset)
        };
        if read == -1 {
            return Err(Error::last_os_error());
        }

        Ok(read as usize)
    }

    fn pwritev(&self, fd: RawFd, offset: i64) -> Result<usize> {
        if !self
            .as_ref()
            .iter()
            .all(|mapping| mapping.prot.contains(Prot::READ))
        {
            return Err(Error::new(
                ErrorKind::PermissionDenied,
                "No read access",
            ));
        }

        let iov = self
            .as_ref()
            .iter()
            .map(|mapping| iovec {
                iov_base: mapping.ptr.as_ptr() as *mut libc::c_void,
                iov_len: mapping.len,
            })
            .collect::<Vec<_>>();

        let written = unsafe {
            libc::pwritev(fd, iov.as_ptr(), iov.len() as libc::c_int, offset)
        };
        if written == -1 {
            return Err(Error::last_os_error());
        }

        Ok(written as usize)
    }
}

/// Wrapper around an address space for a VM.
pub struct MemCtx {
    map: Arc<ASpace<MapEnt>>,
}
impl MemCtx {
    /// Reads a generic value from a specified guest address.
    pub fn read<T: Copy + FromBytes>(
        &self,
        addr: GuestAddr,
    ) -> Option<GuestData<T>> {
        if let Some(mapping) =
            self.region_covered(addr, size_of::<T>(), Prot::READ)
        {
            mapping.read::<T>().ok().map(GuestData::from)
        } else {
            None
        }
    }
    /// Reads bytes into a requested buffer from guest memory.
    ///
    /// Copies up to `buf.len()` or `len` bytes, whichever is smaller.
    pub fn read_into(
        &self,
        addr: GuestAddr,
        buf: &mut GuestData<&mut [u8]>,
        len: usize,
    ) -> Option<usize> {
        let len = usize::min(buf.len(), len);
        if let Some(mapping) = self.region_covered(addr, len, Prot::READ) {
            mapping.read_bytes(&mut buf[..len]).ok()
        } else {
            None
        }
    }
    /// Reads bytes from guest memory into a buffer, using the direct
    /// mapping.
    pub fn direct_read_into(
        &self,
        addr: GuestAddr,
        buf: &mut GuestData<&mut [u8]>,
        len: usize,
    ) -> Option<usize> {
        let len = usize::min(buf.len(), len);
        let region = GuestRegion(addr, len);
        let mapping = self.direct_readable_region(&region)?;
        mapping.read_bytes(&mut buf[..len]).ok()
    }

    /// Reads multiple objects from a guest address.
    pub fn read_many<T: Copy + FromBytes>(
        &self,
        base: GuestAddr,
        count: usize,
    ) -> Option<GuestData<MemMany<'_, T>>> {
        self.region_covered(base, size_of::<T>() * count, Prot::READ).map(
            |mapping| {
                GuestData::from(MemMany {
                    mapping,
                    pos: 0,
                    count,
                    phantom: PhantomData,
                })
            },
        )
    }
    /// Writes a value to guest memory.
    pub fn write<T: Copy>(&self, addr: GuestAddr, val: &T) -> bool {
        if let Some(mapping) =
            self.region_covered(addr, size_of::<T>(), Prot::WRITE)
        {
            mapping.write(val).is_ok()
        } else {
            false
        }
    }
    /// Writes bytes from a buffer to guest memory.
    ///
    /// Writes up to `buf.len()` or `len` bytes, whichever is smaller.
    pub fn write_from(
        &self,
        addr: GuestAddr,
        buf: &[u8],
        len: usize,
    ) -> Option<usize> {
        let len = usize::min(buf.len(), len);
        if let Some(mapping) = self.region_covered(addr, len, Prot::WRITE) {
            mapping.write_bytes(&buf[..len]).ok()
        } else {
            None
        }
    }
    /// Writes a single value to guest memory.
    pub fn write_byte(&self, addr: GuestAddr, val: u8, count: usize) -> bool {
        if let Some(mapping) = self.region_covered(addr, count, Prot::WRITE) {
            mapping.write_byte(val, count).is_ok()
        } else {
            false
        }
    }
    /// Write multiple values to guest memory
    ///
    /// If the memory offset and value(s) size would result in the copy crossing
    /// vmm memory segments, this will fail.
    pub fn write_many<T: Copy>(&self, addr: GuestAddr, val: &[T]) -> bool {
        if let Some(mapping) =
            self.region_covered(addr, size_of_val(val), Prot::WRITE)
        {
            mapping.write_many(val).is_ok()
        } else {
            false
        }
    }

    pub fn writable_region(
        &self,
        region: &GuestRegion,
    ) -> Option<SubMapping<'_>> {
        let mapping = self.region_covered(region.0, region.1, Prot::WRITE)?;
        Some(mapping)
    }
    pub fn readable_region(
        &self,
        region: &GuestRegion,
    ) -> Option<SubMapping<'_>> {
        let mapping = self.region_covered(region.0, region.1, Prot::READ)?;
        Some(mapping)
    }
    pub fn readwrite_region(
        &self,
        region: &GuestRegion,
    ) -> Option<SubMapping<'_>> {
        let mapping = self.region_covered(region.0, region.1, Prot::RW)?;
        Some(mapping)
    }

    /// Like `direct_writable_region`, but looks up the region by name.
    pub fn direct_writable_region_by_name(
        &self,
        name: &str,
    ) -> Result<SubMapping<'_>> {
        let ent = self
            .map
            .iter()
            .find_map(|(_addr, _len, ent)| match &ent.kind {
                MapKind::Dram(seg) if ent.name == name => Some(&seg.map_seg),
                MapKind::Rom(seg) if ent.name == name => Some(&seg.map_seg),
                _ => None,
            })
            .ok_or_else(|| {
                Error::new(
                    ErrorKind::NotFound,
                    format!("memory region {} not found", name),
                )
            })?;
        Ok(SubMapping::new_base(self, ent).constrain_access(Prot::WRITE))
    }

    /// Like `writable_region`, but accesses the underlying memory segment
    /// directly, bypassing protection enforced to the guest and tracking of
    /// dirty pages in the guest-physical address space.
    pub fn direct_writable_region(
        &self,
        region: &GuestRegion,
    ) -> Option<SubMapping<'_>> {
        let (_guest_map, seg_map) = self.region_mappings(region.0, region.1)?;
        Some(seg_map.constrain_access(Prot::WRITE))
    }

    /// Like `readable_region`, but accesses the underlying memory segment
    /// directly, bypassing protection enforced to the guest and tracking of
    /// accessed pages in the guest-physical address space.
    pub fn direct_readable_region(
        &self,
        region: &GuestRegion,
    ) -> Option<SubMapping<'_>> {
        let (_guest_map, seg_map) = self.region_mappings(region.0, region.1)?;
        Some(seg_map.constrain_access(Prot::READ))
    }

    /// Look up a region in the guest's address space and return its protection
    /// (as preceived by the guest) and mapping access, both through the nested
    /// page tables, and directly to the underlying memory segment.
    fn region_mappings(
        &self,
        addr: GuestAddr,
        len: usize,
    ) -> Option<(SubMapping<'_>, SubMapping<'_>)> {
        let start = addr.0 as usize;
        let end = start + len;
        if let Ok((addr, rlen, ent)) = self.map.region_at(start) {
            if addr + rlen < end {
                return None;
            }
            let req_offset = start - addr;
            let (prot, seg) = match &ent.kind {
                MapKind::Dram(seg) => Some((Prot::RW, seg)),
                MapKind::Rom(seg) => Some((Prot::READ, seg)),
                MapKind::MmioReserve => None,
            }?;

            let guest_map = SubMapping::new_base(self, &seg.map_guest)
                .constrain_access(prot)
                .constrain_region(req_offset, len)
                .expect("mapping offset should be valid");

            let seg_map = SubMapping::new_base(self, &seg.map_seg)
                .constrain_region(req_offset, len)
                .expect("mapping offset should be valid");

            return Some((guest_map, seg_map));
        }
        None
    }

    /// Looks up a region of memory in the guest's address space, returning a
    /// pointer to the containing region.
    fn region_covered(
        &self,
        addr: GuestAddr,
        len: usize,
        req_prot: Prot,
    ) -> Option<SubMapping<'_>> {
        let (guest_map, _seg_map) = self.region_mappings(addr, len)?;
        // Although this protection check could be considered redundant with the
        // permissions on the mapping itself, performing it here allows
        // consumers to gracefully handle errors, rather than taking a fault
        // when attempting to exceed the guest's apparent permissions.
        if guest_map.prot().contains(req_prot) {
            Some(guest_map)
        } else {
            None
        }
    }

    /// Returns the [lowest, highest] memory addresses in the space as an
    /// inclusive range.
    pub fn mem_bounds(&self) -> Option<RangeInclusive<GuestAddr>> {
        let lowest = self
            .map
            .lowest_addr(|entry| matches!(entry.kind, MapKind::Dram(_)))?
            as u64;
        let highest = self
            .map
            .highest_addr(|entry| matches!(entry.kind, MapKind::Dram(_)))?
            as u64;
        Some(GuestAddr(lowest)..=GuestAddr(highest))
    }
}

pub enum MemAccessed {}
impl crate::accessors::AccessedResource for MemAccessed {
    type Root = Arc<MemCtx>;
    type Leaf = Arc<MemCtx>;
    type Target = MemCtx;

    fn derive(root: &Self::Root) -> Self::Leaf {
        root.clone()
    }
    fn deref(leaf: &Self::Leaf) -> &Self::Target {
        leaf
    }
}

/// A contiguous region of memory containing generic objects.
pub struct MemMany<'a, T: Copy> {
    mapping: SubMapping<'a>,
    count: usize,
    pos: usize,
    phantom: PhantomData<T>,
}
impl<T: Copy + FromBytes> GuestData<MemMany<'_, T>> {
    /// Gets the object at position `pos` within the memory region.
    ///
    /// Returns [`Option::None`] if out of range.
    pub fn get(&self, pos: usize) -> Option<GuestData<T>> {
        if pos < self.count {
            let sz = std::mem::size_of::<T>();
            self.mapping
                .subregion(pos * sz, sz)?
                .read::<T>()
                .ok()
                .map(GuestData::from)
        } else {
            None
        }
    }
}
impl<T: Copy + FromBytes> Iterator for GuestData<MemMany<'_, T>> {
    type Item = GuestData<T>;

    fn next(&mut self) -> Option<Self::Item> {
        let res = self.get(self.pos);
        self.pos += 1;
        res
    }
}

/// A 52-bit physical page number, i.e., the ordinal index of a 4 KiB page of
/// guest memory.
#[derive(Clone, Copy, Ord, PartialOrd, Eq, PartialEq)]
pub(crate) struct Pfn(u64);

impl std::fmt::Debug for Pfn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Pfn({:#x})", self.0)
    }
}

impl std::fmt::Display for Pfn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        core::fmt::LowerHex::fmt(&self.0, f)
    }
}

impl From<Pfn> for u64 {
    fn from(value: Pfn) -> Self {
        value.0
    }
}

impl Pfn {
    /// Creates a new PFN wrapper for the supplied physical page number. Returns
    /// `None` if the page number cannot correctly be shifted to form the
    /// corresponding 64-bit address.
    pub(crate) fn new(pfn: u64) -> Option<Self> {
        if pfn > (PAGE_MASK as u64 >> PAGE_SHIFT) {
            None
        } else {
            Some(Self(pfn))
        }
    }

    /// Creates a new PFN wrapper without checking that the supplied PFN can
    /// be shifted to produce a corresponding 64-bit address.
    ///
    /// # Safety
    ///
    /// The supplied PFN must fit in 52 bits; otherwise [`Self::addr`] will
    /// return incorrect addresses for this PFN.
    //
    // This is currently only used by test code.
    #[cfg(test)]
    pub(crate) fn new_unchecked(pfn: u64) -> Self {
        Self(pfn)
    }

    /// Yields the 64-bit address corresponding to this PFN.
    pub(crate) fn addr(&self) -> GuestAddr {
        GuestAddr(self.0 << PAGE_SHIFT)
    }
}

#[cfg(test)]
pub mod test {
    use super::*;

    const TEST_LEN: usize = 16 * 1024;

    fn test_setup(prot: Prot) -> (VmmHdl, Arc<Mapping>) {
        let hdl = VmmHdl::new_test(TEST_LEN)
            .expect("create tempfile backed test hdl");
        let base = Mapping::new(TEST_LEN, prot, &hdl, 0).unwrap();
        (hdl, base)
    }

    #[test]
    fn memory_protections_match_libc() {
        assert_eq!(i32::from(Prot::READ.bits()), libc::PROT_READ);
        assert_eq!(i32::from(Prot::WRITE.bits()), libc::PROT_WRITE);
        assert_eq!(i32::from(Prot::EXEC.bits()), libc::PROT_EXEC);
    }

    #[test]
    fn mapping_denies_read_beyond_end() {
        let (_hdl, base) = test_setup(Prot::READ);
        let mapping = SubMapping::new_base_test(base);

        assert!(mapping.read::<[u8; TEST_LEN + 1]>().is_err());
    }

    #[test]
    fn mapping_shortens_read_bytes_beyond_end() {
        let (_hdl, base) = test_setup(Prot::READ);
        let mapping = SubMapping::new_base_test(base);

        let mut buf: [u8; TEST_LEN + 1] = [0; TEST_LEN + 1];
        assert_eq!(TEST_LEN, mapping.read_bytes(&mut buf).unwrap());
    }

    #[test]
    fn mapping_shortens_write_bytes_beyond_end() {
        let (_hdl, base) = test_setup(Prot::RW);
        let mapping = SubMapping::new_base_test(base);

        let mut buf: [u8; TEST_LEN + 1] = [0; TEST_LEN + 1];
        assert_eq!(TEST_LEN, mapping.write_bytes(&mut buf).unwrap());
    }

    #[test]
    fn mapping_create_empty() {
        let (_hdl, base) = test_setup(Prot::READ);
        let mapping =
            SubMapping::new_base_test(base).constrain_region(0, 0).unwrap();

        assert_eq!(0, mapping.len());
        assert!(mapping.is_empty());
    }

    #[test]
    fn mapping_valid_subregions() {
        let (_hdl, base) = test_setup(Prot::READ);
        let mapping = SubMapping::new_base_test(base);

        assert!(mapping.subregion(0, 0).is_some());
        assert!(mapping.subregion(0, TEST_LEN / 2).is_some());
        assert!(mapping.subregion(TEST_LEN, 0).is_some());
    }

    #[test]
    fn mapping_invalid_subregions() {
        let (_hdl, base) = test_setup(Prot::READ);
        let mapping = SubMapping::new_base_test(base);

        // Beyond the end of the mapping.
        assert!(mapping.subregion(TEST_LEN + 1, 0).is_none());
        assert!(mapping.subregion(TEST_LEN, 1).is_none());

        // Overflow.
        assert!(mapping.subregion(usize::MAX, 1).is_none());
        assert!(mapping.subregion(1, usize::MAX).is_none());
    }

    #[test]
    fn subregion_protection() {
        let (_hdl, base) = test_setup(Prot::RW);
        let mapping = SubMapping::new_base_test(base);

        // Main region has full access
        let mut buf = [0u8];
        assert!(mapping.write_bytes(&buf).is_ok());
        assert!(mapping.read_bytes(&mut buf).is_ok());

        // Restricted to reads
        let sub_read = mapping
            .subregion(0, TEST_LEN)
            .unwrap()
            .constrain_access(Prot::READ);
        assert!(sub_read.write_bytes(&buf).is_err());
        assert!(sub_read.read_bytes(&mut buf).is_ok());

        // Restricted to writes
        let sub_write = mapping
            .subregion(0, TEST_LEN)
            .unwrap()
            .constrain_access(Prot::WRITE);
        assert!(sub_write.write_bytes(&buf).is_ok());
        assert!(sub_write.read_bytes(&mut buf).is_err());
    }
}
