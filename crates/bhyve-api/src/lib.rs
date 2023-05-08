use std::fs::{File, OpenOptions};
use std::io::{Error, ErrorKind, Result};
use std::os::fd::*;
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

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
        cache_api_version(|| -> Result<u32> { self.query_api_version() })
    }

    /// Perform the actual query of the API version
    fn query_api_version(&self) -> Result<u32> {
        let vers = self.ioctl_usize(ioctls::VMM_INTERFACE_VERSION, 0)?;

        // We expect and demand a positive version number from the
        // VMM_INTERFACE_VERSION interface.
        assert!(vers > 0);
        Ok(vers as u32)
    }

    /// Request that the VMM memory reservoir be resized.
    ///
    /// Since this may involve gathering a large portion of memory from the OS
    /// kernel to place in the reservoir, a chunking parameter `chunk_bytes` can
    /// be used to limit the size of in-kernel requests used to fulfill the
    /// request.
    pub fn reservoir_resize(
        &self,
        target_bytes: usize,
        chunk_bytes: usize,
    ) -> std::result::Result<(), ReservoirError> {
        let mut req = vmm_resv_target {
            vrt_target_sz: target_bytes,
            vrt_chunk_sz: chunk_bytes,
            ..Default::default()
        };

        // Safety: We are using the appropriate struct for this ioctl
        let res = unsafe { self.ioctl(ioctls::VMM_RESV_SET_TARGET, &mut req) };

        match res {
            Err(e) if e.kind() == ErrorKind::Interrupted => {
                Err(ReservoirError::Interrupted(req.vrt_result_sz))
            }
            Err(e) => Err(ReservoirError::Io(e)),

            Ok(_) => Ok(()),
        }
    }

    /// Query VMM memory reservoir capacity and usage.
    pub fn reservoir_query(&self) -> Result<vmm_resv_query> {
        let mut req = vmm_resv_query::default();

        // Safety: We are using the appropriate struct for this ioctl
        unsafe { self.ioctl(ioctls::VMM_RESV_QUERY, &mut req) }?;

        Ok(req)
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

pub enum ReservoirError {
    /// Resizing operation was interrupted, but if a non-zero chunk size was
    /// specified, one or more chunk-sized adjustments to the reservoir size may
    /// have completed.
    ///
    /// In that case, the resulting size of the reservoir is returned.
    Interrupted(usize),
    /// An IO error (other than interruption) occurred
    Io(Error),
}
impl From<ReservoirError> for Error {
    fn from(val: ReservoirError) -> Self {
        match val {
            ReservoirError::Interrupted(_) => {
                Error::new(ErrorKind::Interrupted, "interrupted")
            }
            ReservoirError::Io(e) => e,
        }
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

    /// Query the API version exposed by the kernel VMM.
    pub fn api_version(&self) -> Result<u32> {
        cache_api_version(|| -> Result<u32> {
            match self.ioctl_usize(ioctls::VMM_INTERFACE_VERSION, 0) {
                Ok(v) => {
                    assert!(v > 0);
                    Ok(v as u32)
                }
                Err(e) if e.raw_os_error() == Some(libc::ENOTTY) => {
                    // Prior to V6, the only the vmmctl device would answer
                    // version queries, so fall back gracefully if the ioctl is
                    // unrecognized.
                    let ctl = VmmCtlFd::open()?;
                    ctl.query_api_version()
                }
                Err(e) => Err(e),
            }
        })
    }

    /// Set the time reported by the virtual RTC time.
    ///
    /// Arguments:
    /// - `time`: Duration since `UNIX_EPOCH`
    pub fn rtc_settime(&self, time: Duration) -> Result<()> {
        if self.api_version()? >= ApiVersion::V12.into() {
            let mut ts = libc::timespec {
                tv_sec: time.as_secs() as i64,
                tv_nsec: time.subsec_nanos() as i64,
            };
            unsafe { self.ioctl(ioctls::VM_RTC_SETTIME, &mut ts) }?;
            Ok(())
        } else {
            // The old RTC_SETTIME only support seconds precision
            let mut time_sec: u64 = time.as_secs();
            unsafe { self.ioctl(ioctls::VM_RTC_SETTIME, &mut time_sec) }?;
            Ok(())
        }
    }

    /// Check VMM ioctl command against those known to not require any
    /// copyin/copyout to function.
    const fn ioctl_usize_safe(cmd: i32) -> bool {
        matches!(
            cmd,
            ioctls::VM_PAUSE
                | ioctls::VM_RESUME
                | ioctls::VM_DESTROY_SELF
                | ioctls::VM_SET_AUTODESTRUCT
                | ioctls::VMM_INTERFACE_VERSION,
        )
    }
}

impl AsRawFd for VmmFd {
    fn as_raw_fd(&self) -> RawFd {
        self.0.as_raw_fd()
    }
}

/// Store a cached copy of the queried API version.  Negative values indicate an
/// error occurred during query (and hold the corresponding negated `errno`).
/// A positive value indicates the cached version, and should be less than
/// `u32::MAX`.  A value of 0 indicates that no query has been performed yet.
static VERSION_CACHE: AtomicI64 = AtomicI64::new(0);

/// Query the API version from the kernel VMM component on the system.
///
/// Caches said version (or any emitted error) for later calls.
pub fn api_version() -> Result<u32> {
    cache_api_version(|| -> Result<u32> {
        let ctl = VmmCtlFd::open()?;
        let vers = ctl.query_api_version()?;
        Ok(vers)
    })
}

fn cache_api_version(do_query: impl FnOnce() -> Result<u32>) -> Result<u32> {
    if VERSION_CACHE.load(Ordering::Acquire) == 0 {
        let newval = match do_query() {
            Ok(x) => x as i64,
            Err(e) => -(e.raw_os_error().unwrap_or(libc::ENOENT) as i64),
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
            assert!(y < u32::MAX as i64);

            Ok(y as u32)
        }
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
/// been introduced in the various bhyve API versions.
#[repr(u32)]
#[derive(IntoPrimitive)]
pub enum ApiVersion {
    /// Writes via vmm-data interface are allowed by default
    V13 = 13,

    /// Improved RTC emulation, including sub-second precision
    V12 = 12,

    /// Add support for modifing guest time data via vmm-data interface
    V11 = 11,

    /// Interrupt and exception state is properly saved/restored on VM
    /// pause/resume, and is exposed via vmm-data interface
    V10 = 10,

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
        Self::V13
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
