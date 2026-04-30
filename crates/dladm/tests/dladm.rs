#![cfg(target_os = "illumos")]

use dladm::Dladm;
use dladm::DladmError;
use std::io::Result;

#[test]
fn empty_link_name() {
    let hnd = Dladm::new().unwrap();

    assert!(hnd.describe_link("").is_err());
}

#[test]
fn query() {
    let hnd = Dladm::new().unwrap();

    let x = hnd.describe_link("vioif0");
    //let x = hnd.describe_link("opte0");
    dbg!(x);
}
