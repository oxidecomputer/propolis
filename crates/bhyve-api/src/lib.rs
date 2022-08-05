mod enums;
mod ioctls;
mod structs;
mod vmm_data;

pub use enums::*;
pub use ioctls::*;
pub use structs::*;
pub use vmm_data::*;

pub const VM_MAXCPU: usize = 32;

pub const VMM_PATH_PREFIX: &str = "/dev/vmm";
pub const VMM_CTL_PATH: &str = "/dev/vmmctl";

/// This is the VMM interface version against which bhyve_api expects to operate
/// against.  All constants and structs defined by the crate are done so in
/// terms of that specific version.
pub const VMM_CURRENT_INTERFACE_VERSION: u32 = 4;

use std::io::Result;

/// Check that bhyve kernel VMM component matches version underlying interfaces
/// defined in bhyve-api.  Can use a user-provided version (via `version` arg)
/// or the current one known to bhyve_api.
#[cfg(target_os = "illumos")]
pub fn check_version(version: Option<u32>) -> Result<bool> {
    use std::fs::OpenOptions;
    use std::io::Error;
    use std::os::unix::fs::OpenOptionsExt;
    use std::os::unix::io::AsRawFd;

    let ctl = OpenOptions::new()
        .write(true)
        .custom_flags(libc::O_EXCL)
        .open(VMM_CTL_PATH)?;
    let ctlfd = ctl.as_raw_fd();
    let res = unsafe {
        libc::ioctl(
            ctlfd,
            VMM_INTERFACE_VERSION,
            std::ptr::null_mut() as *mut libc::c_void,
        )
    };
    if res < 0 {
        return Err(Error::last_os_error());
    }

    let check_against = version.unwrap_or(VMM_CURRENT_INTERFACE_VERSION);
    Ok(check_against == res as u32)
}

#[cfg(not(target_os = "illumos"))]
pub fn check_version(_version: Option<u32>) -> Result<bool> {
    Ok(false)
}
