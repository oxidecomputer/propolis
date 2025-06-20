// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::fs::{File, OpenOptions};
use std::io::{Error, ErrorKind, Result};
use std::mem::{size_of, size_of_val};
use std::os::fd::*;
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

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

    /// Destroy VM instance
    pub fn vm_destroy(&self, name: &[u8]) -> Result<()> {
        let mut req = vm_destroy_req::new(name)?;
        unsafe { self.ioctl(ioctls::VMM_DESTROY_VM, &mut req)? };
        Ok(())
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
        if self.api_version()? >= ApiVersion::V12 as u32 {
            let mut ts = libc::timespec {
                tv_sec: time.as_secs() as i64,
                tv_nsec: i64::from(time.subsec_nanos()),
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

    /// Build a [`VmmDataOp`] with specified `class` and `version` to read or
    /// write data from the in-kernel vmm.
    pub fn data_op(&self, class: u16, version: u16) -> VmmDataOp<'_> {
        VmmDataOp::new(self, class, version)
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
                | ioctls::VMM_INTERFACE_VERSION
                | ioctls::VM_VCPU_BARRIER,
        )
    }
}

impl AsRawFd for VmmFd {
    fn as_raw_fd(&self) -> RawFd {
        self.0.as_raw_fd()
    }
}

pub type VmmDataResult<T> = std::result::Result<T, VmmDataError>;

/// Encompasses the configuration and context to perform a vmm-data operation
/// (read or write) against an instance.  The `class` and `version` for the
/// vmm-data operation are established with the parameters passed to
/// [`VmmFd::data_op`].
pub struct VmmDataOp<'a> {
    fd: &'a VmmFd,
    class: u16,
    version: u16,
    vcpuid: Option<i32>,
}

impl<'a> VmmDataOp<'a> {
    pub fn new(fd: &'a VmmFd, class: u16, version: u16) -> Self {
        Self { fd, class, version, vcpuid: None }
    }
}

impl VmmDataOp<'_> {
    /// Dictate that the vmm-data operation be performed in the context of a
    /// specific vCPU, rather than against the VM as a whole.
    pub fn for_vcpu(mut self, vcpuid: i32) -> Self {
        self.vcpuid = Some(vcpuid);
        self
    }

    /// Read item of data, returning the result.
    pub fn read<T: Sized + Copy + Default>(self) -> VmmDataResult<T> {
        let mut item = T::default();
        self.do_read_single(
            &mut item as *mut T as *mut libc::c_void,
            size_of::<T>() as u32,
            false,
        )?;
        Ok(item)
    }

    /// Read item of data into provided buffer
    pub fn read_into<T: Sized>(self, data: &mut T) -> VmmDataResult<()> {
        self.do_read_single(
            data as *mut T as *mut libc::c_void,
            size_of::<T>() as u32,
            false,
        )
    }

    /// Read item of data, specified by identifier existing in provided buffer
    pub fn read_item<T: Sized>(self, data: &mut T) -> VmmDataResult<()> {
        self.do_read_single(
            data as *mut T as *mut libc::c_void,
            size_of::<T>() as u32,
            true,
        )
    }

    fn do_read_single(
        self,
        data: *mut libc::c_void,
        read_len: u32,
        do_copyin: bool,
    ) -> VmmDataResult<()> {
        let mut xfer = self.xfer_base(read_len, data);

        if do_copyin {
            xfer.vdx_flags |= VDX_FLAG_READ_COPYIN;
        }

        let bytes_read = self.do_read(&mut xfer)?;
        assert_eq!(bytes_read, read_len);
        Ok(())
    }

    /// Read data items, specified by identifiers existing in provided buffer
    pub fn read_many<T: Sized>(self, data: &mut [T]) -> VmmDataResult<()> {
        let read_len = size_of_val(data) as u32;
        let mut xfer =
            self.xfer_base(read_len, data.as_mut_ptr() as *mut libc::c_void);

        // When reading multiple items, it is expected that identifiers will be
        // passed into the kernel to select the entries which will be read (as
        // opposed to read-all, which is indiscriminate).
        //
        // As such, a copyin-before-read is implied to provide said identifiers.
        xfer.vdx_flags |= VDX_FLAG_READ_COPYIN;

        let bytes_read = self.do_read(&mut xfer)?;
        assert_eq!(bytes_read, read_len);
        Ok(())
    }

    /// Read all data items offered by this class/version
    pub fn read_all<T: Sized>(self) -> VmmDataResult<Vec<T>> {
        let mut xfer = self.xfer_base(0, std::ptr::null_mut());
        let total_len = match self.do_read(&mut xfer) {
            Err(VmmDataError::SpaceNeeded(sz)) => Ok(sz),
            Err(e) => Err(e),
            Ok(_) => panic!("unexpected success"),
        }?;
        let item_len = size_of::<T>() as u32;
        assert!(total_len >= item_len, "item size exceeds total data size");

        let item_count = total_len / item_len;
        assert_eq!(
            total_len,
            item_count * item_len,
            "per-item sizing does not match total data size"
        );

        let mut data: Vec<T> = Vec::with_capacity(item_count as usize);
        let mut xfer =
            self.xfer_base(total_len, data.as_mut_ptr() as *mut libc::c_void);

        let bytes_read = self.do_read(&mut xfer)?;
        assert!(bytes_read <= total_len);

        // SAFETY: Data is populated by the ioctl
        unsafe {
            data.set_len((bytes_read / item_len) as usize);
        }
        Ok(data)
    }

    /// Write item of data
    pub fn write<T: Sized>(self, data: &T) -> VmmDataResult<()> {
        let write_len = size_of::<T>() as u32;
        let mut xfer = self.xfer_base(
            write_len,
            data as *const T as *mut T as *mut libc::c_void,
        );

        let bytes_written = self.do_write(&mut xfer)?;
        assert_eq!(bytes_written, write_len);
        Ok(())
    }

    /// Write data items
    pub fn write_many<T: Sized>(self, data: &[T]) -> VmmDataResult<()> {
        let write_len = size_of_val(data) as u32;
        let mut xfer = self
            .xfer_base(write_len, data.as_ptr() as *mut T as *mut libc::c_void);

        let bytes_written = self.do_write(&mut xfer)?;
        assert_eq!(bytes_written, write_len);
        Ok(())
    }

    /// Build a [`vm_data_xfer`] struct based on parameters established for this
    /// data operation.
    fn xfer_base(&self, len: u32, data: *mut libc::c_void) -> vm_data_xfer {
        vm_data_xfer {
            vdx_vcpuid: self.vcpuid.unwrap_or(-1),
            vdx_class: self.class,
            vdx_version: self.version,
            vdx_len: len,
            vdx_data: data,
            ..Default::default()
        }
    }

    fn do_read(
        &self,
        xfer: &mut vm_data_xfer,
    ) -> std::result::Result<u32, VmmDataError> {
        self.do_ioctl(VM_DATA_READ, xfer)
    }

    fn do_write(
        &self,
        xfer: &mut vm_data_xfer,
    ) -> std::result::Result<u32, VmmDataError> {
        // If logic is added to VM_DATA_WRITE which actually makes use of
        // [`VDX_FLAG_WRITE_COPYOUT`], then the fact that [`write`] and
        // [`write_many`] accept const references for the data input will need
        // to be revisited.
        self.do_ioctl(VM_DATA_WRITE, xfer)
    }

    /// Execute a vmm-data transfer, translating the ENOSPC error, if emitted
    fn do_ioctl(
        &self,
        op: i32,
        xfer: &mut vm_data_xfer,
    ) -> std::result::Result<u32, VmmDataError> {
        match unsafe { self.fd.ioctl(op, xfer) } {
            Err(e) => match e.raw_os_error() {
                Some(errno) if errno == libc::ENOSPC => {
                    Err(VmmDataError::SpaceNeeded(xfer.vdx_result_len))
                }
                _ => Err(VmmDataError::IoError(e)),
            },
            Ok(_) => Ok(xfer.vdx_result_len),
        }
    }
}

#[derive(Debug)]
pub enum VmmDataError {
    IoError(Error),
    SpaceNeeded(u32),
}

impl From<VmmDataError> for Error {
    fn from(err: VmmDataError) -> Self {
        match err {
            VmmDataError::IoError(e) => e,
            VmmDataError::SpaceNeeded(c) => {
                // ErrorKind::StorageFull would more accurately match the underlying ENOSPC
                // but that variant is unstable still
                Error::other(format!("operation requires {} bytes", c))
            }
        }
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

/// Convenience constants to provide some documentation on what changes have
/// been introduced in the various bhyve API versions.
#[repr(u32)]
#[derive(Copy, Clone)]
pub enum ApiVersion {
    /// Initial support for CPU perf. counters on AMD
    V18 = 18,

    /// Add support for NPT bitmap operations
    V17 = 17,

    /// VM Suspend behavior reworked, `VM_VCPU_BARRIER` ioctl added
    V16 = 16,

    /// Add flag for exit-when-consistent as part of `VM_RUN`
    V15 = 15,

    /// Reading specific MSRs via vmm-data is fixed.  Access to DEBUGCTL and
    /// LBR-related MSR state is possible (on AMD).
    V14 = 14,

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

    /// Adds ability to control `cpuid` results for guest vCPUs
    V5 = 5,
}
impl ApiVersion {
    pub const fn current() -> Self {
        Self::V18
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

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn latest_api_version() {
        let cur = ApiVersion::current();
        assert_eq!(VMM_CURRENT_INTERFACE_VERSION, cur as u32);
    }

    #[test]
    fn u32_comparisons() {
        assert!(4u32 < ApiVersion::V5);
        assert!(5u32 == ApiVersion::V5);
        assert!(6u32 > ApiVersion::V5);
    }
}
