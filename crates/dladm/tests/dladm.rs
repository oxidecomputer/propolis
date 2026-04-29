#![cfg(target_os = "illumos")]

use dladm::Handle;
use std::io::Result;

#[test]
fn basic() -> Result<()> {
    let hnd = Handle::new();

    Ok(())
}
