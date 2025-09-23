// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("IO Error")]
    Io(#[from] std::io::Error),

    #[error("API version {0} did not match expectation {1}")]
    Mismatch(u32, u32),
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
