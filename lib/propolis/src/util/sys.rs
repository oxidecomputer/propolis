use std::io::{Error, Result};
use std::os::unix::io::RawFd;

#[cfg(not(target_os = "illumos"))]
use std::io::ErrorKind;

// Deal with libc bits which vary enough between OSes to make cargo-check a pain

#[cfg(target_os = "illumos")]
pub fn ioctl<T>(fd: RawFd, cmd: i32, data: *mut T) -> Result<i32> {
    let res = unsafe { libc::ioctl(fd, cmd, data) };
    if res == -1 {
        Err(Error::last_os_error())
    } else {
        Ok(res)
    }
}
#[cfg(not(target_os = "illumos"))]
pub fn ioctl<T>(_fd: RawFd, _cmd: i32, _data: *mut T) -> Result<i32> {
    Err(Error::new(ErrorKind::Other, "illumos required"))
}

#[cfg(target_os = "illumos")]
pub fn ioctl_usize(fd: RawFd, cmd: i32, data: usize) -> Result<i32> {
    let res = unsafe { libc::ioctl(fd, cmd, data) };
    if res == -1 {
        Err(Error::last_os_error())
    } else {
        Ok(res)
    }
}
#[cfg(not(target_os = "illumos"))]
pub fn ioctl_usize(_fd: RawFd, _cmd: i32, _data: usize) -> Result<i32> {
    Err(Error::new(ErrorKind::Other, "illumos required"))
}
