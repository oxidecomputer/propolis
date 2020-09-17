use std::ffi::CString;
use std::fs::File;
use std::io::{Error, ErrorKind, Result};
use std::os::unix::io::AsRawFd;
use std::path::PathBuf;

use bhyve_api;
use libc;

struct VmHdl(File);

fn create_vm(name: &str) -> Result<VmHdl> {
    let ctl = File::open(bhyve_api::VMM_CTL_PATH)?;
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

    let fp = File::open(vmpath)?;
    Ok(VmHdl(fp))
}

