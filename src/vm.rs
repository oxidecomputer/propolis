use core::ptr;
use std::ffi::CString;
use std::fs::{File, OpenOptions};
use std::io::{Error, ErrorKind, Result, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::{AsRawFd, RawFd};
use std::path::PathBuf;

use bhyve_api;
use libc;

#[cfg(target_os = "illumos")]
pub fn create_vm(name: &str) -> Result<VmmHdl> {
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
    Ok(VmmHdl { inner: fp })
}
#[cfg(not(target_os = "illumos"))]
pub fn create_vm(_name: &str) -> Result<VmmHdl> {
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

pub struct VmmHdl {
    inner: File,
}
impl VmmHdl {
    fn fd(&self) -> RawFd {
        self.inner.as_raw_fd()
    }
    #[cfg(target_os = "illumos")]
    pub fn ioctl<T>(&self, cmd: i32, data: *mut T) -> Result<i32> {
        let res = unsafe { libc::ioctl(self.fd(), cmd, data) };
        if res == -1 {
            Err(Error::last_os_error())
        } else {
            Ok(res)
        }
    }
    #[cfg(not(target_os = "illumos"))]
    pub fn ioctl<T>(&self, _cmd: i32, _data: *mut T) -> Result<i32> {
        Err(Error::new(ErrorKind::Other, "illumos required"))
    }

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
        self.ioctl(bhyve_api::VM_ALLOC_MEMSEG, &mut seg)?;
        Ok(())
    }

    pub fn map_memseg(
        &self,
        segid: i32,
        gpa: usize,
        len: usize,
        segoff: usize,
        prot: u8,
    ) -> Result<()> {
        assert!(segoff <= i64::MAX as usize);

        let mut map = bhyve_api::vm_memmap {
            gpa: gpa as u64,
            segid,
            segoff: segoff as i64,
            len,
            prot: prot as i32,
            flags: 0,
        };
        self.ioctl(bhyve_api::VM_MMAP_MEMSEG, &mut map)?;
        Ok(())
    }

    pub fn devmem_offset(&self, segid: i32, offset: usize) -> Result<usize> {
        assert!(offset <= i64::MAX as usize);

        let mut devoff = bhyve_api::vm_devmem_offset { segid, offset: 0 };
        self.ioctl(bhyve_api::VM_DEVMEM_GETOFFSET, &mut devoff)?;

        assert!(devoff.offset >= 0);
        Ok(devoff.offset as usize)
    }

    pub unsafe fn mmap_seg(&self, segid: i32, size: usize) -> Result<*mut u8> {
        let devoff = self.devmem_offset(segid, 0)?;
        let ptr = libc::mmap(
            ptr::null_mut(),
            size,
            libc::PROT_WRITE,
            libc::MAP_SHARED,
            self.fd(),
            devoff as i64,
        ) as *mut u8;
        if ptr.is_null() {
            return Err(Error::last_os_error());
        }
        Ok(ptr)
    }

    pub fn rtc_settime(&self, unix_time: u64) -> Result<()> {
        let mut time: u64 = unix_time;
        self.ioctl(bhyve_api::VM_RTC_SETTIME, &mut time)?;
        Ok(())
    }

    pub fn rtc_write(&self, offset: u8, value: u8) -> Result<()> {
        let mut data = bhyve_api::vm_rtc_data { offset: offset as i32, value };
        self.ioctl(bhyve_api::VM_RTC_WRITE, &mut data)?;
        Ok(())
    }
}
