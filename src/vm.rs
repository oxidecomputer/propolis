use std::ffi::CString;
use std::fs::{File, OpenOptions};
use std::io::{Error, ErrorKind, Result};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::AsRawFd;
use std::path::PathBuf;

use bhyve_api;
use libc;

pub fn create_vm(name: &str) -> Result<VmHdl> {
    let ctl = OpenOptions::new()
        .write(true)
        .custom_flags(libc::O_EXCL)
        .open(bhyve_api::VMM_CTL_PATH)?;
    let namestr = CString::new(name).or_else(|_x| Err(Error::from_raw_os_error(libc::EINVAL)))?;
    let nameptr = namestr.as_ptr();
    let ctlfd = ctl.as_raw_fd();

    let res = unsafe { libc::ioctl(ctlfd, bhyve_api::VMM_CREATE_VM, nameptr) };
    if res != 0 {
        let err = Error::last_os_error();
        if err.kind() != ErrorKind::AlreadyExists {
            return Err(err);
        }
        // try to nuke(!) the existing vm
        let res = unsafe { libc::ioctl(ctlfd, bhyve_api::VMM_DESTROY_VM, nameptr) };
        if res != 0 {
            let err = Error::last_os_error();
            if err.kind() != ErrorKind::NotFound {
                return Err(err);
            }
        }
        // attempt to create in its presumed absence
        let res = unsafe { libc::ioctl(ctlfd, bhyve_api::VMM_CREATE_VM, nameptr) };
        if res != 0 {
            return Err(Error::last_os_error());
        }
    }

    let mut vmpath = PathBuf::from(bhyve_api::VMM_PATH_PREFIX);
    vmpath.push(name);

    let fp = OpenOptions::new().write(true).read(true).open(vmpath)?;
    Ok(VmHdl(fp))
}

#[repr(u8)]
#[allow(non_camel_case_types)]
enum VmMemsegs {
    SEG_LOWMEM,
}

pub struct VmHdl(File);

impl VmHdl {
    pub fn setup_memory(&mut self, size: u64) -> Result<()> {
        let mut seg = bhyve_api::vm_memseg {
            segid: VmMemsegs::SEG_LOWMEM as i32,
            len: size as usize,
            name: [0u8; bhyve_api::SEG_NAME_LEN]
        };
        self.ioctl(bhyve_api::VM_ALLOC_MEMSEG, &mut seg)?;
        let mut map = bhyve_api::vm_memmap {
            gpa: 0,
            segid: VmMemsegs::SEG_LOWMEM as i32,
            segoff: 0,
            len: size as usize,
            prot: bhyve_api::PROT_ALL as i32,
            flags: 0,
        };
        self.ioctl(bhyve_api::VM_MMAP_MEMSEG, &mut map)?;
        Ok(())
    }
    fn ioctl<T>(&self, cmd: i32, data: *mut T) -> Result<i32> {
        let fd = self.0.as_raw_fd();
        let res = unsafe {
            libc::ioctl(fd, cmd, data)
        };
        if res == -1 {
            Err(Error::last_os_error())
        } else {
            Ok(res)
        }
    }
}
