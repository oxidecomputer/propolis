// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("IO Error")]
    Io(#[from] std::io::Error),

    // Newer APIs are backwards compatible with older (for now?), so don't
    // bother trying to express "we need a version before .." or any of that.
    //
    // Also assume that the component being versioned is either part of the OS
    // or that the OS version is a decent proxy for it. Not necessarily true in
    // general, but true for viona and bhyve.
    #[error("API version {have} is not at or above {want}. OS is too old?")]
    TooLow { have: u32, want: u32 },
}

#[derive(Debug, thiserror::Error)]
#[error("checking version of {component}")]
pub struct VersionCheckError {
    pub component: &'static str,
    pub path: &'static str,
    #[source]
    pub err: Error,
}

impl VersionCheckError {
    fn vmm(err: Error) -> Self {
        Self { component: "vmm", path: bhyve_api::VMM_CTL_PATH, err }
    }

    fn viona(err: Error) -> Self {
        Self { component: "viona", path: viona_api::VIONA_DEV_PATH, err }
    }
}

pub fn check() -> Result<(), VersionCheckError> {
    crate::vmm::check_api_version().map_err(VersionCheckError::vmm)?;
    crate::hw::virtio::viona::check_api_version()
        .map_err(VersionCheckError::viona)?;

    Ok(())
}
