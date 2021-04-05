//! Module responsible for communicating with the kernel's VMM.
//!
//! Responsible for both issuing commands to the bhyve
//! kernel controller to create and destroy VMs.
//!
//! Additionally, contains a wrapper struct ([`VmmHdl`])
//! for encapsulating commands to the underlying kernel
//! object which represents a single VM.

use std::io::{Error, ErrorKind, Result};
use std::marker::PhantomData;
use std::os::unix::io::RawFd;
use std::ptr::{copy_nonoverlapping, NonNull};

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

/// Provides a level of abstraction above a region of memory mapped
/// via mmap.
///
/// When dealing with raw pointers from, extreme caution must be taken
/// to dereference the pointer safely:
/// - The pointer must not be null
/// - The dereferenced pointer must be within bounds of a valid mapping
/// -
pub struct Mapping {
    inner: SubMapping<'static>,
}

impl Mapping {
    // TODO: still not safe, what about:
    // - Truncating fd?
    // - Size of map > size of object?
    pub fn new(addr: *mut libc::c_void, size: usize, prot: Prot,
               flags: i32, fd: RawFd, devoff: i64
        ) -> Result<Self> {

        let ptr = unsafe {
            libc::mmap(
                addr,
                size,
                prot.bits() as i32,
                flags,
                fd,
                devoff,
            ) as *mut u8
        };
        let ptr = NonNull::new(ptr).ok_or_else(|| Error::last_os_error())?;
        let m = Mapping {
            inner: SubMapping {
                ptr,
                len: size,
                prot,
                _phantom: PhantomData,
            }
        };
        Ok(m)
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        let map = self.as_ref();
        unsafe {
            libc::munmap(map.ptr.as_ptr() as *mut libc::c_void, map.len);
        }
    }
}

// TODO: not safe to concurrent mutation - is that a problem?
// or do we accept this as inevitable because mmap?

#[derive(Debug)]
pub struct SubMapping<'a> {
    ptr: NonNull<u8>,
    len: usize,
    prot: Prot,
    _phantom: PhantomData<&'a ()>,
}

// SAFETY: SubMapping's API does not provide raw access to the underlying
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
    pub fn subregion(&self, offset: usize, length: usize) -> Option<SubMapping> {
        let end = offset.checked_add(length)?;
        if self.len < end {
            return None
        }

        // Safety:
        // - Starting and resulting pointer must be within bounds or
        // one past the end of the same allocated object.
        // - The computed offset, in bytes, cannot overflow isize.
        // - The offset cannot rely on "wrapping around" the address
        // space.
        let ptr = NonNull::new(unsafe { self.ptr.as_ptr().add(offset) }).unwrap();

        let sub = SubMapping {
            ptr,
            len: length,
            prot: self.prot,
            _phantom: PhantomData,
        };

        Some(sub)
    }

    pub fn read<T: Copy>(&self) -> Result<T> {
        if !self.prot.contains(Prot::READ) {
            return Err(Error::new(ErrorKind::PermissionDenied, "No read access"));
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

    pub fn read_bytes(&self, buf: &mut [u8]) -> Result<usize> {
        if !self.prot.contains(Prot::READ) {
            return Err(Error::new(ErrorKind::PermissionDenied, "No read access"));
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

    /// Pread from `fd` into the mapping.
    pub fn pread(&self, fd: RawFd, length: usize, offset: i64) -> Result<usize> {
        if !self.prot.contains(Prot::WRITE) {
            return Err(Error::new(ErrorKind::PermissionDenied, "No read access"));
        }

        let to_read = usize::min(length, self.len);
        let read = unsafe {
            libc::pread(fd, self.ptr.as_ptr() as *mut libc::c_void, to_read, offset)
        };
        if read == -1 {
            return Err(Error::last_os_error());
        }
        Ok(read as usize)
    }

    pub fn write<T: Copy>(&self, value: &T) -> Result<()>{
        if !self.prot.contains(Prot::WRITE) {
            return Err(Error::new(ErrorKind::PermissionDenied, "No write access"));
        }
        let typed = self.ptr.as_ptr() as *mut T;
        unsafe {
            typed.write_unaligned(*value);
        }
        Ok(())
    }

    pub fn write_bytes(&self, buf: &[u8]) -> Result<usize> {
        if !self.prot.contains(Prot::WRITE) {
            return Err(Error::new(ErrorKind::PermissionDenied, "No write access"));
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

    pub fn write_byte(&self, val: u8, count: usize) -> Result<usize> {
        if !self.prot.contains(Prot::WRITE) {
            return Err(Error::new(ErrorKind::PermissionDenied, "No write access"));
        }
        let to_copy = usize::min(count, self.len);
        unsafe {
            self.ptr.as_ptr().write_bytes(val, to_copy);
        }
        Ok(to_copy)
    }

    /// Pwrite from the mapping to `fd`.
    pub fn pwrite(&self, fd: RawFd, length: usize, offset: i64) -> Result<usize> {
        if !self.prot.contains(Prot::READ) {
            return Err(Error::new(ErrorKind::PermissionDenied, "No write access"));
        }

        let to_write = usize::min(length, self.len);
        let written = unsafe {
            libc::pwrite(fd, self.ptr.as_ptr() as *const libc::c_void, to_write, offset)
        };
        if written == -1 {
            return Err(Error::last_os_error());
        }
        Ok(written as usize)
    }

    pub fn len(&self) -> usize {
        self.len
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assert_protections() {
        assert_eq!(Prot::READ.bits() as i32, libc::PROT_READ);
        assert_eq!(Prot::WRITE.bits() as i32, libc::PROT_WRITE);
        assert_eq!(Prot::EXEC.bits() as i32, libc::PROT_EXEC);
    }

    /*

    #[test]
    fn mapping_lifetime() {
        // TODO: Will need to patch this with drop...
        let ptr = NonNull::new(0x1234 as *mut u8).unwrap();
        let mapping = unsafe { Mapping::new(ptr, 1024) };
        mapping.as_ref().subregion(512);
    }

    */
}
