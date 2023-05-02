use anyhow::{anyhow, Result};

pub struct VmmStateWriteGuard;

pub fn set_vmm_globals() -> Result<Vec<Box<dyn std::any::Any>>> {
    Err(anyhow!("set_vmm_globals requires illumos"))
}
