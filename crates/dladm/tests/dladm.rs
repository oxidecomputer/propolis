#![cfg(target_os = "illumos")]

use dladm::Dladm;

#[test]
fn empty_link_name() {
    let hnd = Dladm::new().unwrap();

    assert!(hnd.describe_link("").is_err());
}

#[test]
fn bogus_name() {
    let hnd = Dladm::new().unwrap();

    assert!(hnd.describe_link("nonsense").is_err());
}

#[test]
fn query() {
    let hnd = Dladm::new().unwrap();

    let opte0 = hnd.describe_link("opte0").unwrap();

    assert_eq!(opte0.mtu.unwrap(), 1500);
}
