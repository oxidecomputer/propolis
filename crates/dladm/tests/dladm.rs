#![cfg(target_os = "illumos")]

use dladm::Dladm;
use std::io::Result;

#[test]
fn basic() -> Result<()> {
    let hnd = Dladm::new();

    Ok(())
}
