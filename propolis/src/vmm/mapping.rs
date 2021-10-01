//! Module for managing guest memory mappings.

use libc::iovec;

use crate::common::PAGE_SIZE;
use crate::util::aspace::ASpace;
use crate::vmm::VmmFile;

use std::fs::File;
use std::io::{Error, ErrorKind, Result};
use std::marker::PhantomData;
use std::mem::ManuallyDrop;
use std::os::unix::io::{AsRawFd, RawFd};
use std::ptr::{copy_nonoverlapping, NonNull};

// 2MB guard length
/// The size of a guard page.
pub const GUARD_LEN: usize = 0x20000;
pub const GUARD_ALIGN: usize = 0x20000;

#[cfg(target_os = "illumos")]
const FLAGS_MAP_GUARD: i32 =
    libc::MAP_ANON | libc::MAP_PRIVATE | libc::MAP_NORESERVE | libc::MAP_ALIGN;
#[cfg(not(target_os = "illumos"))]
const FLAGS_MAP_GUARD: i32 =
    libc::MAP_ANON | libc::MAP_PRIVATE | libc::MAP_NORESERVE;

bitflags! {
    /// Bitflags representing memory protections.
    pub struct Prot: u8 {
        const NONE = 0;
        const READ = bhyve_api::PROT_READ as u8;
        const WRITE = bhyve_api::PROT_WRITE as u8;
        const EXEC = bhyve_api::PROT_EXEC as u8;
        const ALL = (bhyve_api::PROT_READ
                    | bhyve_api::PROT_WRITE
                    | bhyve_api::PROT_EXEC) as u8;
    }
}

/// A region of memory, bounded by two guard pages.
pub struct GuardSpace {
    // Original PROT_NONE mapping, which is replaced by other mappings.
    //
    // Portions of map are vended out to callers during the usage of
    // GuardSpace, though the remaining "unused" portions are unmapped
    // manually in GuardSpace's drop implementation.
    map: ManuallyDrop<Mapping>,

    // Tracks the allocated mappings within the GuardSpace,
    // relative to the start address of the first guard page.
    aspace: ASpace<()>,
}

impl GuardSpace {
    /// Creates a new guard region, capable of storing a mapping of the
    /// requested size.
    ///
    /// # Arguments
    /// - `size`: The size of the mapping, not including guard pages.
    /// Implicitly rounded up to the nearest [`GUARD_LEN`].
    pub fn new(size: usize) -> Result<GuardSpace> {
        let prot = Prot::NONE;

        // Round up size to the nearest GUARD_LEN.
        let padded = (size + (GUARD_LEN - 1)) & !(GUARD_LEN - 1);
        // Total size is the user-accessible space, plus pages on either side.
        let overall = GUARD_LEN * 2 + padded;

        // Safety: This invocation of mmap isn't safe only because of FFI;
        // it isn't requesting a fixed address mapping, and uses anonymous
        // (rather than file-backed) virtual memory.
        let ptr = unsafe {
            libc::mmap(
                GUARD_ALIGN as *mut libc::c_void,
                overall,
                prot.bits().into(),
                FLAGS_MAP_GUARD,
                -1,
                0,
            ) as *mut u8
        };
        let ptr = NonNull::new(ptr).ok_or_else(Error::last_os_error)?;

        let mut aspace = ASpace::new(0, overall);

        // Register the two guard pages.
        aspace.register(0, GUARD_LEN, ()).unwrap();
        aspace.register(overall - GUARD_LEN, GUARD_LEN, ()).unwrap();

        Ok(GuardSpace {
            map: ManuallyDrop::new(Mapping {
                inner: SubMapping {
                    ptr,
                    len: overall,
                    prot,
                    _phantom: PhantomData,
                },
            }),
            aspace,
        })
    }

    /// Creates a new mapping within the bounds of the guard region, replacing
    /// guard pages with the new mapping.
    ///
    /// `size` must be divisible by [`PAGE_SIZE`].
    ///
    /// The lifetime of the returned mapping can exceed the lifetime of the
    /// GuardSpace - dropping the GuardSpace early merely removes the guard
    /// mappings.
    pub fn mapping(
        &mut self,
        size: usize,
        prot: Prot,
        vmm: &VmmFile,
        devoff: i64,
    ) -> Result<Mapping> {
        if size % PAGE_SIZE != 0 {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "Size not aligned to page size",
            ));
        }

        // Find free space large enough for this mapping.
        //
        // This acts as a first-fit allocator.
        let free_space = self
            .aspace
            .inverse_iter()
            .find(|&extent| extent.len() >= size)
            .ok_or_else(|| {
                Error::new(ErrorKind::NotFound, "Not enough guard space")
            })?;

        // Access the to-be-mapped subregion.
        let subregion =
            self.map.as_ref().subregion(free_space.start(), size).ok_or_else(
                || Error::new(ErrorKind::NotFound, "Not enough guard space"),
            )?;

        // Safety: The region of memory being replaced by MAP_FIXED has been
        // allocated by the GuardSpace, and becomes inaccessible to other
        // callers after this invocation succeeds.
        let mapping = unsafe {
            Mapping::new_internal(Some(subregion.ptr), size, prot, vmm, devoff)?
        };
        self.aspace.register(free_space.start(), size, ()).unwrap();
        Ok(mapping)
    }
}

impl Drop for GuardSpace {
    fn drop(&mut self) {
        // Deregister the guard pages, as we would like to unmap them.
        self.aspace.unregister(0).unwrap();
        self.aspace.unregister(self.map.as_ref().len() - GUARD_LEN).unwrap();

        // Unmap all space marked "free" in the original mapping.
        // Other regions are used by Mapping objects, and will be
        // unmapped when those mappings go out of scope.
        for free_space in self.aspace.inverse_iter() {
            let r = unsafe {
                let start =
                    self.map.as_ref().ptr.as_ptr().add(free_space.start());
                libc::munmap(start as *mut libc::c_void, free_space.len())
            };
            assert!(r == 0, "Unmap of GuardSpace failed");
        }
    }
}

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
pub struct Mapping {
    inner: SubMapping<'static>,
}

impl Mapping {
    /// Creates a new memory mapping from a VmmFile, with the requested
    /// permissions.
    pub fn new(
        size: usize,
        prot: Prot,
        vmm: &VmmFile,
        devoff: i64,
    ) -> Result<Self> {
        // Safety: addr == None, so the invocation may choose its own mapping.
        unsafe { Mapping::new_internal(None, size, prot, vmm, devoff) }
    }

    // Safety:
    // - If addr != None, the caller must ensure that the region of memory
    // from [addr, addr + size) has previously been mapped with Prot::None.
    // Using mmap with MAP_FIXED silently replaces conflicting pages, so
    // pointing to an arbitrary address risks colliding with the rest of the
    // address space.
    // - The creator of the VmmFile is responsible for ensuring it points
    // to an object that may not be truncated. If this property is upheld,
    // the returned mapping cannot suddenly become invalided.
    // - The returned region of memory must not be accessed via reference,
    // as it is accessible to the guest, which may arbitrarily read or
    // write the region.
    unsafe fn new_internal(
        addr: Option<NonNull<u8>>,
        size: usize,
        prot: Prot,
        vmm: &VmmFile,
        devoff: i64,
    ) -> Result<Self> {
        let flags =
            libc::MAP_SHARED | if addr.is_some() { libc::MAP_FIXED } else { 0 };

        let addr = addr
            .map(|addr| addr.as_ptr() as *mut libc::c_void)
            .unwrap_or_else(core::ptr::null_mut);

        let ptr =
            libc::mmap(addr, size, prot.bits().into(), flags, vmm.fd(), devoff)
                as *mut u8;
        let ptr = NonNull::new(ptr).ok_or_else(Error::last_os_error)?;
        let m = Mapping {
            inner: SubMapping { ptr, len: size, prot, _phantom: PhantomData },
        };
        Ok(m)
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        let map = self.as_ref();
        // Safety:
        // - No references may exist to the mapping at the time it is dropped,
        // as no references are created.
        // - No child mappings (SubMappings) of the original should exist, as
        // they have shorter lifetimes.
        unsafe {
            libc::munmap(map.ptr.as_ptr() as *mut libc::c_void, map.len);
        }
    }
}

/// A borrowed region from a [`Mapping`] object.
///
/// Provides interfaces for acting on memory, but does not own the
/// underlying memory region.
#[derive(Debug)]
pub struct SubMapping<'a> {
    ptr: NonNull<u8>,
    len: usize,
    prot: Prot,
    _phantom: PhantomData<&'a ()>,
}

// Safety: SubMapping's API does not provide raw access to the underlying
// pointer, nor any mechanism to create references to the underlying data.
unsafe impl<'a> Send for SubMapping<'a> {}
unsafe impl<'a> Sync for SubMapping<'a> {}

impl<'a> AsRef<SubMapping<'a>> for Mapping {
    fn as_ref(&self) -> &SubMapping<'a> {
        &self.inner
    }
}

impl<'a> SubMapping<'a> {
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
    ) -> Option<SubMapping> {
        let end = offset.checked_add(length)?;
        if self.len < end {
            return None;
        }

        // Safety:
        // - Starting and resulting pointer must be within bounds or
        // one past the end of the same allocated object.
        // - The computed offset, in bytes, cannot overflow isize.
        // - The offset cannot rely on "wrapping around" the address
        // space.
        let ptr =
            NonNull::new(unsafe { self.ptr.as_ptr().add(offset) }).unwrap();

        let sub = SubMapping {
            ptr,
            len: length,
            prot: self.prot,
            _phantom: PhantomData,
        };

        Some(sub)
    }

    /// Reads a `T` object from the mapping.
    pub fn read<T: Copy>(&self) -> Result<T> {
        if !self.prot.contains(Prot::READ) {
            return Err(Error::new(
                ErrorKind::PermissionDenied,
                "No read access",
            ));
        }
        let typed = self.ptr.as_ptr() as *const T;
        if self.len < std::mem::size_of::<T>() {
            return Err(Error::new(ErrorKind::InvalidData, "Buffer too small"));
        }

        // Safety:
        // - typed must be valid for reads
        // - typed must point to a properly initialized value of T
        Ok(unsafe { typed.read_unaligned() })
    }

    /// Reads a buffer of bytes from the mapping.
    pub fn read_bytes(&self, buf: &mut [u8]) -> Result<usize> {
        if !self.prot.contains(Prot::READ) {
            return Err(Error::new(
                ErrorKind::PermissionDenied,
                "No read access",
            ));
        }
        let to_copy = usize::min(buf.len(), self.len);
        let src = self.ptr.as_ptr();
        let dst = buf.as_mut_ptr();

        // Safety:
        // - src must be valid for reads of to_copy * size_of::<u8>() bytes.
        // - dst must be valid for writes of count * size_of::<u8>() bytes.
        // - Both src and dst must be properly aligned.
        // - The region of memory beginning at src with a size of count *
        // size_of::<u8>() bytes must not overlap with the region of memory beginning
        // at dst with the same size.
        unsafe {
            copy_nonoverlapping(src, dst, to_copy);
        }
        Ok(to_copy)
    }

    /// Pread from `file` into the mapping.
    pub fn pread(
        &self,
        file: &File,
        length: usize,
        offset: i64,
    ) -> Result<usize> {
        if !self.prot.contains(Prot::WRITE) {
            return Err(Error::new(
                ErrorKind::PermissionDenied,
                "No write access",
            ));
        }

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
        if !self.prot.contains(Prot::WRITE) {
            return Err(Error::new(
                ErrorKind::PermissionDenied,
                "No write access",
            ));
        }
        let typed = self.ptr.as_ptr() as *mut T;
        unsafe {
            typed.write_unaligned(*value);
        }
        Ok(())
    }

    /// Writes a buffer of bytes into the mapping.
    pub fn write_bytes(&self, buf: &[u8]) -> Result<usize> {
        if !self.prot.contains(Prot::WRITE) {
            return Err(Error::new(
                ErrorKind::PermissionDenied,
                "No write access",
            ));
        }

        let to_copy = usize::min(buf.len(), self.len);
        let src = buf.as_ptr();
        let dst = self.ptr.as_ptr();

        // Safety:
        // - src must be valid for reads of count * size_of::<T>() bytes.
        // - dst must be valid for writes of count * size_of::<T>() bytes.
        // - Both src and dst must be properly aligned.
        // - The region of memory beginning at src with a size of count *
        // size_of::<T>() bytes must not overlap with the region of memory beginning
        // at dst with the same size.
        unsafe {
            copy_nonoverlapping(src, dst, to_copy);
        }
        Ok(to_copy)
    }

    /// Writes a single byte `val` to the mapping, `count` times.
    pub fn write_byte(&self, val: u8, count: usize) -> Result<usize> {
        if !self.prot.contains(Prot::WRITE) {
            return Err(Error::new(
                ErrorKind::PermissionDenied,
                "No write access",
            ));
        }
        let to_copy = usize::min(count, self.len);
        unsafe {
            self.ptr.as_ptr().write_bytes(val, to_copy);
        }
        Ok(to_copy)
    }

    /// Pwrite from the mapping to `file`.
    pub fn pwrite(
        &self,
        file: &File,
        length: usize,
        offset: i64,
    ) -> Result<usize> {
        if !self.prot.contains(Prot::READ) {
            return Err(Error::new(
                ErrorKind::PermissionDenied,
                "No write access",
            ));
        }

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
            Some(self.ptr.as_ptr() as *mut u8)
        } else {
            None
        }
    }
}

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

#[cfg(test)]
pub mod tests {
    use super::*;
    use tempfile::tempfile;

    pub fn test_vmm(len: u64) -> VmmFile {
        let file = tempfile().unwrap();
        file.set_len(len).unwrap();
        unsafe { VmmFile::new(file) }
    }

    #[test]
    fn memory_protections_match_libc() {
        assert_eq!(Prot::READ.bits() as i32, libc::PROT_READ);
        assert_eq!(Prot::WRITE.bits() as i32, libc::PROT_WRITE);
        assert_eq!(Prot::EXEC.bits() as i32, libc::PROT_EXEC);
    }

    #[test]
    fn guard_space_creates_readable_writable_regions() {
        let mut guard = GuardSpace::new(GUARD_LEN).unwrap();
        let vmm = test_vmm(GUARD_LEN as u64);
        let mapping = guard
            .mapping(GUARD_LEN, Prot::READ | Prot::WRITE, &vmm, 0)
            .unwrap();

        let input: u64 = 0xDEADBEEF;
        mapping.as_ref().write(&input).unwrap();
        let output = mapping.as_ref().read().unwrap();
        assert_eq!(input, output);
    }

    #[test]
    fn guard_space_cannot_allocate_beyond_end() {
        let mut guard = GuardSpace::new(GUARD_LEN).unwrap();
        let vmm = test_vmm(GUARD_LEN as u64);

        let _ = guard
            .mapping(GUARD_LEN, Prot::READ | Prot::WRITE, &vmm, 0)
            .unwrap();
        // No space remaining after the first allocation.
        assert!(guard
            .mapping(GUARD_LEN, Prot::READ | Prot::WRITE, &vmm, 0)
            .is_err());
    }

    #[test]
    fn guard_space_must_allocate_modulo_page_size() {
        let mut guard = GuardSpace::new(GUARD_LEN).unwrap();
        let vmm = test_vmm(GUARD_LEN as u64);
        assert!(guard.mapping(PAGE_SIZE - 1, Prot::READ, &vmm, 0).is_err());
    }

    #[test]
    fn mapping_denies_read_beyond_end() {
        let vmm = test_vmm(GUARD_LEN as u64);
        let mapping = Mapping::new(GUARD_LEN, Prot::READ, &vmm, 0).unwrap();

        assert!(mapping.as_ref().read::<[u8; GUARD_LEN + 1]>().is_err());
    }

    #[test]
    fn mapping_shortens_read_bytes_beyond_end() {
        let vmm = test_vmm(GUARD_LEN as u64);
        let mapping = Mapping::new(GUARD_LEN, Prot::READ, &vmm, 0).unwrap();

        let mut buf: [u8; GUARD_LEN + 1] = [0; GUARD_LEN + 1];
        assert_eq!(GUARD_LEN, mapping.as_ref().read_bytes(&mut buf).unwrap());
    }

    #[test]
    fn mapping_create_empty() {
        let vmm = test_vmm(GUARD_LEN as u64);
        let mapping = Mapping::new(0, Prot::READ, &vmm, 0).unwrap();

        assert_eq!(0, mapping.as_ref().len());
        assert!(mapping.as_ref().is_empty());
    }

    #[test]
    fn mapping_valid_subregions() {
        let vmm = test_vmm(GUARD_LEN as u64);
        let mapping = Mapping::new(GUARD_LEN, Prot::READ, &vmm, 0).unwrap();

        assert!(mapping.as_ref().subregion(0, 0).is_some());
        assert!(mapping.as_ref().subregion(0, GUARD_LEN / 2).is_some());
        assert!(mapping.as_ref().subregion(GUARD_LEN, 0).is_some());
    }

    #[test]
    fn mapping_invalid_subregions() {
        let vmm = test_vmm(GUARD_LEN as u64);
        let mapping = Mapping::new(GUARD_LEN, Prot::READ, &vmm, 0).unwrap();

        // Beyond the end of the mapping.
        assert!(mapping.as_ref().subregion(GUARD_LEN + 1, 0).is_none());
        assert!(mapping.as_ref().subregion(GUARD_LEN, 1).is_none());

        // Overflow.
        assert!(mapping.as_ref().subregion(usize::MAX, 1).is_none());
        assert!(mapping.as_ref().subregion(1, usize::MAX).is_none());
    }
}
