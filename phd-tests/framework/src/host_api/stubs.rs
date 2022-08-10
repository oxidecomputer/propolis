use anyhow::{anyhow, Result};

pub struct VmmStateWriteGuard;

pub fn enable_vmm_state_writes() -> Result<VmmStateWriteGuard> {
    Err(anyhow!("enable_vmm_state_writes requires illumos"))
}
