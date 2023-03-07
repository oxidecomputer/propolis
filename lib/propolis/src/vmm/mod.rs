//! Representation of a VM's hardware and kernel structures.

pub mod data;
pub mod hdl;
pub mod machine;
pub mod mem;

pub use hdl::*;
pub use machine::*;
pub use mem::*;

/// Check that available vmm API matches expectations of propolis crate
pub(crate) fn check_api_version() -> Result<(), crate::api_version::Error> {
    let ctl = bhyve_api::VmmCtlFd::open()?;
    let vers = ctl.api_version()?;

    // propolis only requires the bits provided by V8, currently
    let compare = bhyve_api::ApiVersion::V8.into();

    if vers < compare {
        return Err(crate::api_version::Error::Mismatch("vmm", vers, compare));
    }

    Ok(())
}
