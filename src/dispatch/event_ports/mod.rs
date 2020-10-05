use std::os::unix::io::RawFd;

#[cfg(target_os = "illumos")]
mod illumos;
#[cfg(target_os = "illumos")]
pub use illumos::*;

// Keep a compatibility shim around so cargo-check still works elsewhere
#[cfg(not(target_os = "illumos"))]
mod compat;
#[cfg(not(target_os = "illumos"))]
pub use compat::*;

pub enum EventObj {
    Fd(RawFd, FdEvents),
    User(usize, i32),
}

pub struct EventResult {
    pub obj: EventObj,
    pub data: usize,
}

bitflags! {
    /// Events relevant to Fd-type event sources.
    ///
    /// The POLLERR, POLLHUP, and POLLNVAL events are not to be used during
    /// port.associate() calls, but are expected only as emitted events during
    pub struct FdEvents: i32 {
        /// Data (other than high priority) may be read
        const POLLIN = libc::POLLIN as i32;
        /// High priority data may be received
        const POLLPRI = libc::POLLPRI as i32;
        /// Normal priority data may be written
        const POLLOUT = libc::POLLOUT as i32;
        /// Normal data may be read
        const POLLRDNORM = libc::POLLRDNORM as i32;
        /// High priority data may be read
        const POLLRDBAND = libc::POLLRDBAND as i32;
        /// High priority data may be written
        const POLLWRBAND = libc::POLLWRBAND as i32;

        /// An error has occurred on the resource
        const POLLERR = libc::POLLERR as i32;
        /// An hangup has occurred on the resource
        const POLLHUP = libc::POLLHUP as i32;
        /// Specified resource dow not belong to open file
        const POLLNVAL = libc::POLLNVAL as i32;
    }
}
