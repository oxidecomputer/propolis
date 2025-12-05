// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::fs::{File, OpenOptions};
use std::io::{Error, ErrorKind, Result};
use std::os::fd::*;
use std::os::unix::fs::MetadataExt;

pub use viona_api_sys::*;

// Hide libnvpair usage when not building on illumos to avoid linking errors
#[cfg(target_os = "illumos")]
pub use nvpair::NvList;

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

    #[cfg(target_os = "illumos")]
    pub fn set_parameters(
        &self,
        params: &mut NvList,
    ) -> std::result::Result<(), ParamError> {
        let mut errbuf: Vec<u8> = Vec::with_capacity(VIONA_MAX_PARAM_NVLIST_SZ);

        let mut packed = params.pack();
        let vsp_param_sz = packed.as_ref().len();

        let mut ioc = vioc_set_params {
            vsp_param: packed.as_mut_ptr().cast(),
            vsp_param_sz,
            vsp_error: errbuf.as_mut_ptr().cast(),
            vsp_error_sz: errbuf.capacity(),
        };
        match unsafe { self.ioctl(VNA_IOC_SET_PARAMS, &mut ioc) } {
            Ok(_) if ioc.vsp_error_sz == 0 => Ok(()),
            Ok(_) => {
                assert!(ioc.vsp_error_sz <= errbuf.capacity());
                unsafe { errbuf.set_len(ioc.vsp_error_sz) };

                match NvList::unpack(&mut errbuf[..]) {
                    Ok(detail) => Err(ParamError::Detailed(detail)),
                    Err(e) => Err(ParamError::Io(e)),
                }
            }
            Err(e) => Err(ParamError::Io(e)),
        }
    }

    #[cfg(target_os = "illumos")]
    pub fn get_parameters(&self) -> Result<NvList> {
        let mut buf: Vec<u8> = Vec::with_capacity(VIONA_MAX_PARAM_NVLIST_SZ);

        let mut ioc = vioc_get_params {
            vgp_param: buf.as_mut_ptr().cast(),
            vgp_param_sz: buf.capacity(),
        };
        let _ = unsafe { self.ioctl(VNA_IOC_GET_PARAMS, &mut ioc) }?;

        assert!(ioc.vgp_param_sz <= buf.capacity());
        unsafe { buf.set_len(ioc.vgp_param_sz) };

        NvList::unpack(&mut buf[..])
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

    /// Retrieve the minor number of the viona device instance.
    /// This is used for matching kernel statistic entries to the viona device.
    pub fn instance_id(&self) -> Result<u32> {
        let meta = self.0.metadata()?;
        Ok(minor(&meta))
    }

    /// Check viona ioctl command against those known to not require any
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
                | ioctls::VNA_IOC_SET_NOTIFY_IOP
                | ioctls::VNA_IOC_SET_PROMISC
                | ioctls::VNA_IOC_GET_MTU
                | ioctls::VNA_IOC_SET_MTU,
        )
    }
}
impl AsRawFd for VionaFd {
    fn as_raw_fd(&self) -> RawFd {
        self.0.as_raw_fd()
    }
}

#[cfg(target_os = "illumos")]
pub enum ParamError {
    Io(std::io::Error),
    Detailed(NvList),
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
    Err(Error::other("illumos required"))
}

#[cfg(target_os = "illumos")]
fn minor(meta: &std::fs::Metadata) -> u32 {
    // With #4208 backported into libc-0.2, minor() became a const-fn for
    // practically all of the UNIX-y platforms, save for illumos.
    //
    // Until we address that, just paper over it with a wrapper here.
    // Viona is not usable anywhere but illumos.
    unsafe { libc::minor(meta.rdev()) }
}
#[cfg(not(target_os = "illumos"))]
fn minor(meta: &std::fs::Metadata) -> u32 {
    let _rdev = meta.rdev();
    panic!("illumos required");
}

/// Convenience constants to provide some documentation on what changes have
/// been introduced in the various viona API versions.
#[repr(u32)]
#[derive(Copy, Clone)]
pub enum ApiVersion {
    /// Add support for getting/setting MTU
    V4 = 4,

    /// Adds support for interface parameters
    V3 = 3,

    /// Adds support for non-vnic datalink devices
    V2 = 2,

    /// Initial version available for query
    V1 = 1,
}
impl ApiVersion {
    pub const fn current() -> Self {
        Self::V4
    }
}
impl PartialEq<ApiVersion> for u32 {
    fn eq(&self, other: &ApiVersion) -> bool {
        *self == *other as u32
    }
}
impl PartialOrd<ApiVersion> for u32 {
    fn partial_cmp(&self, other: &ApiVersion) -> Option<std::cmp::Ordering> {
        Some(self.cmp(&(*other as u32)))
    }
}

use std::sync::atomic::{AtomicI64, Ordering};

/// Store a cached copy of the queried API version.  Negative values indicate an
/// error occurred during query (and hold the corresponding negated `errno`).
/// A positive value indicates the cached version, and should be less than
/// `u32::MAX`.  A value of 0 indicates that no query has been performed yet.
static VERSION_CACHE: AtomicI64 = AtomicI64::new(0);

/// Query the API version from the viona device on the system.
///
/// Caches said version (or any emitted error) for later calls. The API version
/// may be used at runtime in operating the virtual machine, where the delay to
/// query again would be more directly guest-impactful.
pub fn api_version() -> Result<u32> {
    cache_api_version(|| -> Result<u32> {
        let ctl = VionaFd::open()?;
        let vers = ctl.api_version()?;
        Ok(vers)
    })
}

fn cache_api_version(do_query: impl FnOnce() -> Result<u32>) -> Result<u32> {
    if VERSION_CACHE.load(Ordering::Acquire) == 0 {
        let newval = match do_query() {
            Ok(x) => i64::from(x),
            Err(e) => -i64::from(e.raw_os_error().unwrap_or(libc::ENOENT)),
        };
        let _ = VERSION_CACHE.compare_exchange(
            0,
            newval,
            Ordering::Relaxed,
            Ordering::Relaxed,
        );
    }

    match VERSION_CACHE.load(Ordering::Acquire) {
        0 => {
            panic!("expected VERSION_CACHE to be initialized")
        }
        x if x < 0 => Err(Error::from_raw_os_error(-x as i32)),
        y => {
            assert!(y < i64::from(u32::MAX));

            Ok(y as u32)
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn latest_api_version() {
        let cur = ApiVersion::current();
        assert_eq!(VIONA_CURRENT_INTERFACE_VERSION, cur as u32);
    }

    #[test]
    fn u32_comparisons() {
        assert!(1u32 < ApiVersion::V2);
        assert!(2u32 == ApiVersion::V2);
        assert!(3u32 > ApiVersion::V2);
    }
}
