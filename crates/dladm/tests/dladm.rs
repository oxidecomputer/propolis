#![cfg(target_os = "illumos")]

use dladm::Dladm;
use dladm::DladmError;
use std::io::Result;

#[test]
fn empty_link_name() {
    let hnd = Dladm::new().unwrap();

    assert!(matches!(hnd.query_link(""), Err(DladmError::DladmSubsystem(_))));
}

#[test]
fn query() {
    let hnd = Dladm::new().unwrap();

    let x = hnd.query_link("opte0");
    dbg!(x);
}
