// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

pub mod aspace;
pub mod id;
pub mod regmap;

mod ioctl {
    use std::io::{Error, Result};
    use std::os::fd::RawFd;
    use std::os::raw::c_void;

    pub(crate) unsafe fn ioctl(
        fd: RawFd,
        cmd: i32,
        data: *mut c_void,
    ) -> Result<i32> {
        // Paper over differing type for request parameter
        match libc::ioctl(fd, cmd as _, data) {
            -1 => Err(Error::last_os_error()),
            res => Ok(res),
        }
    }
}
pub(crate) use ioctl::ioctl;
