// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("IO Error")]
    Io(#[from] std::io::Error),

    #[error("{0} API version {1} did not match expectation {2}")]
    Mismatch(&'static str, u32, u32),
}

pub fn check() -> Result<(), Error> {
    crate::vmm::check_api_version()?;
    crate::hw::virtio::viona::check_api_version()?;

    Ok(())
}
