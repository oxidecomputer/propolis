#![cfg(target_os = "illumos")]

use dladm::Dladm;
use std::io::Result;

#[test]
fn basic() {
    let hnd = Dladm::new().expect("the inquisition");

    let x = hnd.query_link("opte0");
    dbg!(x);
}
