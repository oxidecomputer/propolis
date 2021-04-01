//! Module responsible for communicating with the kernel's VMM.
//!
//! Responsible for both issuing commands to the bhyve
//! kernel controller to create and destroy VMs.
//!
//! Additionally, contains a wrapper struct ([`VmmHdl`])
//! for encapsulating commands to the underlying kernel
//! object which represents a single VM.

use core::ptr;
use std::ffi::CString;
use std::fs::{File, OpenOptions};
use std::io::{Error, ErrorKind, Result, Write};
use std::marker::PhantomData;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::{AsRawFd, RawFd};
use std::path::PathBuf;
use std::ptr::{copy_nonoverlapping, NonNull};

use crate::util::sys::ioctl;

/// Creates a new virtual machine with the provided `name`.
///
/// Operates on the bhyve controller object at `/dev/vmmctl`,
/// which acts as an interface to the kernel module, and opens
/// an object at `/dev/vmm/{name}`.
///
/// # Arguments
/// - `name`: The name of the VM to create.
/// - `force`: If a VM with the name `name` already exists, attempt
/// to destroy the VM before creating it.
pub fn create_vm(name: impl AsRef<str>, force: bool) -> Result<VmmHdl> {
    create_vm_impl(name.as_ref(), force)
}

#[cfg(target_os = "illumos")]
fn create_vm_impl(name: &str, force: bool) -> Result<VmmHdl> {
    let ctl = OpenOptions::new()
        .write(true)
        .custom_flags(libc::O_EXCL)
        .open(bhyve_api::VMM_CTL_PATH)?;
    let namestr = CString::new(name)
        .or_else(|_x| Err(Error::from_raw_os_error(libc::EINVAL)))?;
    let nameptr = namestr.as_ptr();
    let ctlfd = ctl.as_raw_fd();

    let res = unsafe { libc::ioctl(ctlfd, bhyve_api::VMM_CREATE_VM, nameptr) };
    if res != 0 {
        let err = Error::last_os_error();
        if err.kind() != ErrorKind::AlreadyExists {
            return Err(err);
        } else if !force {
            return Err(err);
        }
        // try to nuke(!) the existing vm
        let res =
            unsafe { libc::ioctl(ctlfd, bhyve_api::VMM_DESTROY_VM, nameptr) };
        if res != 0 {
            let err = Error::last_os_error();
            if err.kind() != ErrorKind::NotFound {
                return Err(err);
            }
        }
        // attempt to create in its presumed absence
        let res =
            unsafe { libc::ioctl(ctlfd, bhyve_api::VMM_CREATE_VM, nameptr) };
        if res != 0 {
            return Err(Error::last_os_error());
        }
    }

    let mut vmpath = PathBuf::from(bhyve_api::VMM_PATH_PREFIX);
    vmpath.push(name);

    let fp = OpenOptions::new().write(true).read(true).open(vmpath)?;
    Ok(VmmHdl { inner: fp, name: name.to_string() })
}
#[cfg(not(target_os = "illumos"))]
fn create_vm_impl(_name: &str, _force: bool) -> Result<VmmHdl> {
    {
        // suppress unused warnings
        let mut _oo = OpenOptions::new();
        _oo.mode(0o444);
        let _cstr = CString::new("");
        let _flag = libc::O_EXCL;
        let _pathbuf = PathBuf::new();
    }
    Err(Error::new(ErrorKind::Other, "illumos required"))
}

/// Destroys the virtual machine matching the provided `name`.
pub fn destroy_vm(name: impl AsRef<str>) -> Result<()> {
    destroy_vm_impl(name.as_ref())
}

#[cfg(target_os = "illumos")]
fn destroy_vm_impl(name: &str) -> Result<()> {
    let ctl = OpenOptions::new()
        .write(true)
        .custom_flags(libc::O_EXCL)
        .open(bhyve_api::VMM_CTL_PATH)?;
    let namestr = CString::new(name)
        .or_else(|_x| Err(Error::from_raw_os_error(libc::EINVAL)))?;
    let nameptr = namestr.as_ptr();
    let ctlfd = ctl.as_raw_fd();

    let res = unsafe { libc::ioctl(ctlfd, bhyve_api::VMM_DESTROY_VM, nameptr) };
    if res != 0 {
        let err = Error::last_os_error();
        if err.kind() == ErrorKind::NotFound {
            return Ok(());
        }
        return Err(err);
    }
    Ok(())
}
#[cfg(not(target_os = "illumos"))]
fn destroy_vm_impl(_name: &str) -> Result<()> {
    Ok(())
}

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
//
// TODO: Possibly remember readability vs writability.
// This is dup'd with the regions in memctx, but really *known*
// at mmap invocation.
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
        Ok(
            Mapping {
                inner: SubMapping {
                    ptr,
                    len: size,
                    prot,
                    _phantom: PhantomData,
                }
            }
        )
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
    /// Returns `None` if the requested offset extends beyond
    /// the end of the mapping.
    pub fn subregion(&self, offset: usize) -> Option<SubMapping> {
        if self.len <= offset {
            return None
        }
        Some(
            SubMapping {
                // Safety:
                // - Starting and resulting pointer must be within bounds or
                // one past the end of the same allocated object.
                // - The computed offset, in bytes, cannot overflow isize.
                // - The offset cannot rely on "wrapping around" the address
                // space.
                ptr: NonNull::new(unsafe { self.ptr.as_ptr().add(offset) }).unwrap(),
                len: self.len - offset,
                prot: self.prot,
                _phantom: PhantomData,
            }
        )
    }

    pub fn set_length(&mut self, length: usize) {
        if self.len >= length {
            return;
        }
        self.len = length;
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

/// A handle to an existing virtual machine monitor.
pub struct VmmHdl {
    inner: File,
    name: String,
}
impl VmmHdl {
    /// Accesses the raw file descriptor behind the VMM.
    pub fn fd(&self) -> RawFd {
        self.inner.as_raw_fd()
    }
    /// Sends an ioctl to the underlying VMM.
    pub fn ioctl<T>(&self, cmd: i32, data: *mut T) -> Result<()> {
        ioctl(self.fd(), cmd, data)?;
        Ok(())
    }
    /// Creates and sends a request to allocate a memory segment within the VM.
    ///
    /// # Arguments
    ///
    /// - `segid`: The segment ID of the requested memory.
    /// - `size`: The size of the memory region, in bytes.
    /// - `segname`: The (optional) name of the memory segment.
    pub fn create_memseg(
        &self,
        segid: i32,
        size: usize,
        segname: Option<&str>,
    ) -> Result<()> {
        let mut seg = bhyve_api::vm_memseg {
            segid,
            len: size,
            name: [0u8; bhyve_api::SEG_NAME_LEN],
        };
        if let Some(name) = segname {
            let name_raw = name.as_bytes();

            assert!(name_raw.len() < bhyve_api::SEG_NAME_LEN);
            (&mut seg.name[..]).write_all(name_raw)?;
        }
        self.ioctl(bhyve_api::VM_ALLOC_MEMSEG, &mut seg)
    }

    /// Maps a memory segment within the guest address space.
    ///
    /// # Arguments
    /// - `segid`: The segment ID to be mapped.
    /// - `gpa`: The "Guest Physical Address" to be mapped.
    /// - `len`: The length of the mapping, in bytes. Must be page aligned.
    /// - `segoff`: Offset within the `gpa` where the mapping should occur.
    /// Must be page aligned.
    /// - `prot`: Memory protections to apply to the guest mapping.
    pub fn map_memseg(
        &self,
        segid: i32,
        gpa: usize,
        len: usize,
        segoff: usize,
        prot: Prot,
    ) -> Result<()> {
        assert!(segoff <= i64::MAX as usize);

        let mut map = bhyve_api::vm_memmap {
            gpa: gpa as u64,
            segid,
            segoff: segoff as i64,
            len,
            prot: prot.bits() as i32,
            flags: 0,
        };
        self.ioctl(bhyve_api::VM_MMAP_MEMSEG, &mut map)
    }

    /// Looks up a segment by `segid` and returns the offset
    /// within the guest's address virtual address space where
    /// it is mapped.
    pub fn devmem_offset(&self, segid: i32) -> Result<usize> {
        let mut devoff = bhyve_api::vm_devmem_offset { segid, offset: 0 };
        self.ioctl(bhyve_api::VM_DEVMEM_GETOFFSET, &mut devoff)?;

        assert!(devoff.offset >= 0);
        Ok(devoff.offset as usize)
    }

    /// Maps a memory segment into propolis' address space.
    ///
    /// Returns a pointer to the mapped segment, if successful.
    pub fn mmap_seg(&self, segid: i32, size: usize) -> Result<Mapping> {
        let devoff = self.devmem_offset(segid)?;

        let prot = Prot::WRITE;
        Mapping::new(ptr::null_mut(),
                     size,
                     prot,
                     libc::MAP_SHARED,
                     self.fd(),
                     devoff as i64)
    }

    /// Maps a portion of the guest's virtual address space
    /// into propolis' address space.
    ///
    /// Returns a pointer to the mapped segment, if successful.
    pub fn mmap_guest_mem(
        &self,
        offset: usize,
        size: usize,
        prot: Prot,
        map_at: Option<NonNull<u8>>,
    ) -> Result<Mapping> {
        let (map_addr, add_flags) = if let Some(addr) = map_at {
            (addr.as_ptr() as *mut libc::c_void, libc::MAP_FIXED)
        } else {
            (ptr::null_mut(), 0)
        };
        Mapping::new(map_addr,
                     size,
                     prot,
                     libc::MAP_SHARED | add_flags,
                     self.fd(),
                     offset as i64)
    }

    /// Issues a request to update the virtual RTC time.
    pub fn rtc_settime(&self, unix_time: u64) -> Result<()> {
        let mut time: u64 = unix_time;
        self.ioctl(bhyve_api::VM_RTC_SETTIME, &mut time)
    }
    /// Writes to the registers within the RTC device.
    pub fn rtc_write(&self, offset: u8, value: u8) -> Result<()> {
        let mut data = bhyve_api::vm_rtc_data { offset: offset as i32, value };
        self.ioctl(bhyve_api::VM_RTC_WRITE, &mut data)
    }

    /// Asserts the requested IRQ for the virtual interrupt controller.
    ///
    /// `pic_irq` sends a request to the legacy 8259 PIC.
    /// `ioapic_irq` (if supplied) sends a request to the IOAPIC.
    pub fn isa_assert_irq(
        &self,
        pic_irq: u8,
        ioapic_irq: Option<u8>,
    ) -> Result<()> {
        let mut data = bhyve_api::vm_isa_irq {
            atpic_irq: pic_irq as i32,
            ioapic_irq: ioapic_irq.map(|x| x as i32).unwrap_or(-1),
        };
        self.ioctl(bhyve_api::VM_ISA_ASSERT_IRQ, &mut data)
    }
    /// Deasserts the requested IRQ.
    pub fn isa_deassert_irq(
        &self,
        pic_irq: u8,
        ioapic_irq: Option<u8>,
    ) -> Result<()> {
        let mut data = bhyve_api::vm_isa_irq {
            atpic_irq: pic_irq as i32,
            ioapic_irq: ioapic_irq.map(|x| x as i32).unwrap_or(-1),
        };
        self.ioctl(bhyve_api::VM_ISA_DEASSERT_IRQ, &mut data)
    }
    /// Pulses the requested IRQ, turning it on then off.
    pub fn isa_pulse_irq(
        &self,
        pic_irq: u8,
        ioapic_irq: Option<u8>,
    ) -> Result<()> {
        let mut data = bhyve_api::vm_isa_irq {
            atpic_irq: pic_irq as i32,
            ioapic_irq: ioapic_irq.map(|x| x as i32).unwrap_or(-1),
        };
        self.ioctl(bhyve_api::VM_ISA_PULSE_IRQ, &mut data)
    }
    #[allow(unused)]
    pub fn isa_set_trigger_mode(
        &self,
        vec: u8,
        level_mode: bool,
    ) -> Result<()> {
        let mut data = bhyve_api::vm_isa_irq_trigger {
            atpic_irq: vec as i32,
            trigger: if level_mode { 1 } else { 0 },
        };
        self.ioctl(bhyve_api::VM_ISA_SET_IRQ_TRIGGER, &mut data)
    }

    #[allow(unused)]
    pub fn ioapic_assert_irq(&self, irq: u8) -> Result<()> {
        let mut data = bhyve_api::vm_ioapic_irq { irq: irq as i32 };
        self.ioctl(bhyve_api::VM_IOAPIC_ASSERT_IRQ, &mut data)
    }
    #[allow(unused)]
    pub fn ioapic_deassert_irq(&self, irq: u8) -> Result<()> {
        let mut data = bhyve_api::vm_ioapic_irq { irq: irq as i32 };
        self.ioctl(bhyve_api::VM_IOAPIC_DEASSERT_IRQ, &mut data)
    }
    #[allow(unused)]
    pub fn ioapic_pulse_irq(&self, irq: u8) -> Result<()> {
        let mut data = bhyve_api::vm_ioapic_irq { irq: irq as i32 };
        self.ioctl(bhyve_api::VM_IOAPIC_PULSE_IRQ, &mut data)
    }
    #[allow(unused)]
    pub fn ioapic_pin_count(&self) -> Result<u8> {
        let mut data = 0u32;
        self.ioctl(bhyve_api::VM_IOAPIC_PINCOUNT, &mut data)?;
        Ok(data as u8)
    }

    pub fn lapic_msi(&self, addr: u64, msg: u64) -> Result<()> {
        let mut data = bhyve_api::vm_lapic_msi { msg, addr };
        self.ioctl(bhyve_api::VM_LAPIC_MSI, &mut data)
    }

    pub fn pmtmr_locate(&self, port: u16) -> Result<()> {
        self.ioctl(bhyve_api::VM_PMTMR_LOCATE, port as *mut usize)
    }

    /// Destroys the VMM.
    // TODO: Should this take "mut self", to consume the object?
    pub fn destroy(&mut self) -> Result<()> {
        destroy_vm(&self.name)
    }
}
