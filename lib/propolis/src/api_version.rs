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
