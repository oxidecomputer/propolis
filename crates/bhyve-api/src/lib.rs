use std::fs::{File, OpenOptions};
use std::io::{Error, ErrorKind, Result};
use std::os::fd::*;
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;

use num_enum::IntoPrimitive;

pub use bhyve_api_sys::*;

pub const VMM_PATH_PREFIX: &str = "/dev/vmm";
pub const VMM_CTL_PATH: &str = "/dev/vmmctl";

pub struct VmmCtlFd(File);
impl VmmCtlFd {
    pub fn open() -> Result<Self> {
        let ctl = OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_EXCL)
            .open(VMM_CTL_PATH)?;
        Ok(Self(ctl))
    }

    /// Issue ioctl against open vmmctl handle
    ///
    /// # Safety
    ///
    /// Caller is charged with providing `data` argument which is adequate for
    /// any copyin/copyout actions which may occur as part of the ioctl
    /// processing.
    pub unsafe fn ioctl<T>(&self, cmd: i32, data: *mut T) -> Result<i32> {
        ioctl(self.as_raw_fd(), cmd, data as *mut libc::c_void)
    }
    pub fn ioctl_usize(&self, cmd: i32, data: usize) -> Result<i32> {
        if !Self::ioctl_usize_safe(cmd) {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "unsafe cmd provided",
            ));
        }
        // Safety: Since we are explicitly filtering for vmm ioctls which will
        // not assume the data argument is a pointer for copyin/copyout, we can
        // dismiss those dangers.  The caller is assumed to be cognizant of
        // other potential side effects.
        unsafe { ioctl(self.as_raw_fd(), cmd, data as *mut libc::c_void) }
    }

    /// Query the API version exposed by the kernel VMM.
    pub fn api_version(&self) -> Result<u32> {
        let vers = self.ioctl_usize(ioctls::VMM_INTERFACE_VERSION, 0)?;

        // We expect and demand a positive version number from the
        // VMM_INTERFACE_VERSION interface.
        assert!(vers > 0);
        Ok(vers as u32)
    }

    /// Check VMM ioctl command against those known to not require any
    /// copyin/copyout to function.
    const fn ioctl_usize_safe(cmd: i32) -> bool {
        matches!(cmd, ioctls::VMM_INTERFACE_VERSION,)
    }
}

impl AsRawFd for VmmCtlFd {
    fn as_raw_fd(&self) -> RawFd {
        self.0.as_raw_fd()
    }
}

pub struct VmmFd(File);
impl VmmFd {
    pub fn open(name: &str) -> Result<Self> {
        let mut vmpath = PathBuf::from(VMM_PATH_PREFIX);
        vmpath.push(name);

        let fp = OpenOptions::new().write(true).read(true).open(vmpath)?;
        Ok(Self(fp))
    }

    /// Create new instance from raw `File` resource
    ///
    /// # Safety
    ///
    /// Caller is expected to provide `File` resource which which is a valid vmm
    /// resource.  (Or alternatively, is not to make any vmm-related ioctls, if
    /// this instance was created for unit-testing purposes.)
    pub unsafe fn new_raw(fp: File) -> Self {
        Self(fp)
    }

    /// Issue ioctl against open vmm instance
    ///
    /// # Safety
    ///
    /// Caller is charged with providing `data` argument which is adequate for
    /// any copyin/copyout actions which may occur as part of the ioctl
    /// processing.
    pub unsafe fn ioctl<T>(&self, cmd: i32, data: *mut T) -> Result<i32> {
        ioctl(self.as_raw_fd(), cmd, data as *mut libc::c_void)
    }
    pub fn ioctl_usize(&self, cmd: i32, data: usize) -> Result<i32> {
        if !Self::ioctl_usize_safe(cmd) {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "unsafe cmd provided",
            ));
        }
        // Safety: Since we are explicitly filtering for vmm ioctls which will
        // not assume the data argument is a pointer for copyin/copyout, we can
        // dismiss those dangers.  The caller is assumed to be cognizant of
        // other potential side effects.
        unsafe { ioctl(self.as_raw_fd(), cmd, data as *mut libc::c_void) }
    }

    /// Check VMM ioctl command against those known to not require any
    /// copyin/copyout to function.
    const fn ioctl_usize_safe(cmd: i32) -> bool {
        matches!(
            cmd,
            ioctls::VM_PAUSE
                | ioctls::VM_RESUME
                | ioctls::VM_DESTROY_SELF
                | ioctls::VM_SET_AUTODESTRUCT,
        )
    }
}

impl AsRawFd for VmmFd {
    fn as_raw_fd(&self) -> RawFd {
        self.0.as_raw_fd()
    }
}

/// Check that bhyve kernel VMM component matches version underlying interfaces
/// defined in bhyve-api.  Can use a user-provided version (via `version` arg)
/// or the current one known to bhyve_api.
pub fn check_version(version: Option<u32>) -> Result<bool> {
    let ctl = VmmCtlFd::open()?;
    let current_vers = unsafe {
        ctl.ioctl(
            ioctls::VMM_INTERFACE_VERSION,
            std::ptr::null_mut() as *mut libc::c_void,
        )
    }?;

    Ok(current_vers as u32 == version.unwrap_or(VMM_CURRENT_INTERFACE_VERSION))
}

#[cfg(target_os = "illumos")]
unsafe fn ioctl(fd: RawFd, cmd: i32, data: *mut libc::c_void) -> Result<i32> {
    match libc::ioctl(fd, cmd, data) {
        -1 => Err(Error::last_os_error()),
        other => Ok(other),
    }
}

#[cfg(not(target_os = "illumos"))]
unsafe fn ioctl(
    _fd: RawFd,
    _cmd: i32,
    _data: *mut libc::c_void,
) -> Result<i32> {
    Err(Error::new(ErrorKind::Other, "illumos required"))
}

/// Convenience constants to provide some documentation on what changes have
/// been introduced in the various bhyve API versions.
#[repr(u32)]
#[derive(IntoPrimitive)]
pub enum ApiVersion {
    /// Revamps ioctls for administrating the VMM memory reservoir and adds
    /// kstat for tracking its capacity and utilization.
    V9 = 9,

    /// Adds flag to enable dirty page tracking for VMs when running on hardware
    /// with adequate support.
    V8 = 8,

    /// Adds pause/resume ioctls to assist with the ability to load or store a
    /// consistent snapshot of VM state
    V7 = 7,

    /// Made hlt-on-exit a required CPU feature, and enabled by default in vmm
    V6 = 6,

    /// Adds ability to control CPUID results for guest vCPUs
    V5 = 5,
}
impl ApiVersion {
    pub const fn current() -> Self {
        Self::V9
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn latest_api_version() {
        let cur = ApiVersion::current();
        assert_eq!(VMM_CURRENT_INTERFACE_VERSION, cur.into());
    }
}
