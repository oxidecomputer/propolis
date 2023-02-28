//! Representation of a VM's hardware and kernel structures.

pub mod data;
pub mod hdl;
pub mod machine;
pub mod mem;

pub use hdl::*;
pub use machine::*;
pub use mem::*;

pub mod version {
    #[derive(Debug, thiserror::Error)]
    pub enum VersionError {
        #[error("IO Error")]
        Io(#[from] std::io::Error),

        #[error("vmm API version {0} did not match expectation {1}")]
        Mismatch(u32, u32),
    }

    /// Check that available vmm API matches expectations of propolis crate
    pub fn check() -> Result<(), VersionError> {
        let ctl = bhyve_api::VmmCtlFd::open()?;

        let vers = ctl.api_version()?;

        // propolis only requires the bits provided by V8, currently
        let compare = bhyve_api::ApiVersion::V8.into();

        if vers < compare {
            Err(VersionError::Mismatch(vers, compare))
        } else {
            Ok(())
        }
    }
}
