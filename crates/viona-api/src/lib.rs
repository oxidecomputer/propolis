// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::fs::{File, OpenOptions};
use std::io::{Error, ErrorKind, Result};
use std::os::fd::*;

use num_enum::IntoPrimitive;

pub use viona_api_sys::*;

pub const VIONA_DEV_PATH: &str = "/dev/viona";

pub struct VionaFd(File);
impl VionaFd {
    /// Open viona device and associate it with a given link and vmm instance,
    /// provided in `link_id` and `vm_fd`, respectively.
    pub fn new(link_id: u32, vm_fd: RawFd) -> Result<Self> {
        let this = Self::open()?;

        let mut vna_create = vioc_create { c_linkid: link_id, c_vmfd: vm_fd };
        let _ = unsafe { this.ioctl(ioctls::VNA_IOC_CREATE, &mut vna_create) }?;
        Ok(this)
    }

    /// Open viona device instance without performing any other initialization
    pub fn open() -> Result<Self> {
        let fp =
            OpenOptions::new().read(true).write(true).open(VIONA_DEV_PATH)?;

        Ok(Self(fp))
    }

    /// Issue ioctl against open viona instance
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
        let vers = self.ioctl_usize(ioctls::VNA_IOC_VERSION, 0)?;

        // We expect and demand a positive version number from the
        // VNA_IOC_VERSION interface.
        assert!(vers > 0);
        Ok(vers as u32)
    }

    /// Check VMM ioctl command against those known to not require any
    /// copyin/copyout to function.
    const fn ioctl_usize_safe(cmd: i32) -> bool {
        matches!(
            cmd,
            ioctls::VNA_IOC_DELETE
                | ioctls::VNA_IOC_RING_RESET
                | ioctls::VNA_IOC_RING_KICK
                | ioctls::VNA_IOC_RING_PAUSE
                | ioctls::VNA_IOC_RING_INTR_CLR
                | ioctls::VNA_IOC_VERSION
        )
    }
}
impl AsRawFd for VionaFd {
    fn as_raw_fd(&self) -> RawFd {
        self.0.as_raw_fd()
    }
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
/// been introduced in the various viona API versions.
#[repr(u32)]
#[derive(IntoPrimitive)]
pub enum ApiVersion {
    /// Adds support for non-vnic datalink devices
    V2 = 2,

    /// Initial version available for query
    V1 = 1,
}
impl ApiVersion {
    pub const fn current() -> Self {
        Self::V2
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn latest_api_version() {
        let cur = ApiVersion::current();
        assert_eq!(VIONA_CURRENT_INTERFACE_VERSION, cur.into());
    }
}
