// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// C-style type names follow, opt out of warnings for using names from headers.
#![allow(non_camel_case_types)]

//! Utility functions for binding LWPs to specific CPUs.
//!
//! This is generally a very light wrapper for illumos' `sysconf(3c)` and
//! `processor_bind(2)`, plus a few constants out of related headers.

use std::io::Error;

// From `<sys/types.h>`
pub type id_t = i32;

// From `<sys/processor.h>`
pub type processorid_t = i32;

// From `<sys/procset.h>`
pub type idtype_t = i32;

/// The enum values `idtype_t` can be. This is separate to be more explicit that
/// idtype_t is the ABI type, but is `repr(i32)` to make casting to `idtype_t`
/// trivial.
#[allow(non_camel_case_types)]
#[repr(i32)]
pub enum IdType {
    P_PID,
    P_PPID,
    P_PGID,
    P_SID,
    P_CID,
    P_UID,
    P_GID,
    P_ALL,
    P_LWPID,
    P_TASKID,
    P_PROJID,
    P_POOLID,
    P_ZONEID,
    P_CTID,
    P_CPUID,
    P_PSETID,
}

// Returns an `i32` to match `processorid_t`, so that `0..online_cpus()`
// produces a range of processor IDs without additional translation needed.
pub fn online_cpus() -> Result<i32, Error> {
    let res = unsafe { libc::sysconf(libc::_SC_NPROCESSORS_ONLN) };

    if res == -1 {
        return Err(Error::last_os_error());
    }

    res.try_into().map_err(|_| {
        // sysconf() reports more than 2^31 processors?!
        Error::other(format!("too many processors: {}", res))
    })
}

#[cfg(target_os = "illumos")]
/// Bind the current LWP to the specified processor.
pub fn bind_lwp(bind_cpu: processorid_t) -> Result<(), Error> {
    extern "C" {
        fn processor_bind(
            idtype: idtype_t,
            id: id_t,
            processorid: processorid_t,
            obind: *mut processorid_t,
        ) -> i32;
    }

    // From `<sys/types.h>`.
    const P_MYID: id_t = -1;

    let res = unsafe {
        processor_bind(
            IdType::P_LWPID as i32,
            P_MYID,
            bind_cpu,
            std::ptr::null_mut(),
        )
    };

    if res != 0 {
        return Err(Error::last_os_error());
    }

    Ok(())
}

#[cfg(not(target_os = "illumos"))]
/// On non-illumos targets, we're not actually running a VM. We do need the
/// crate to compile to be nicer for blanket `cargo test` invocations on other
/// platforms. So a no-op function will do.
pub fn bind_lwp(_bind_cpu: processorid_t) -> Result<(), Error> {
    Ok(())
}
