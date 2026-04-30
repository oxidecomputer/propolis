#![cfg(target_os = "illumos")]

use dladm::datalink_class_t;
use dladm::DlAdmOpt;
use dladm::DlMediaType;
use dladm::Dladm;
use dladm::DlpiMediaType;

#[test]
#[ignore]
fn empty_link_name() {
    let hnd = Dladm::new().unwrap();

    assert!(hnd.describe_link("").is_err());
}

#[test]
#[ignore]
fn bogus_name() {
    let hnd = Dladm::new().unwrap();

    assert!(hnd.describe_link("nonsense").is_err());
}

#[test]
#[ignore]
fn query() {
    let hnd = Dladm::new().unwrap();

    let opte0 = hnd.describe_link("opte0").unwrap();

    assert_eq!(opte0.mtu.unwrap(), 1500);
    assert_eq!(opte0.class, datalink_class_t::DATALINK_CLASS_MISC);
    assert_eq!(opte0.flags, DlAdmOpt::ACTIVE);
    assert_eq!(opte0.media, DlpiMediaType::Known(DlMediaType::Ether));
}

#[test]
#[ignore]
fn query_long_name() {
    let hnd = Dladm::new().unwrap();

    // To try out:
    // dladm create-etherstub -t aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa0

    let iface = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa0";

    assert_eq!(iface.as_bytes().len(), 31);

    let link = hnd.describe_link(iface).unwrap();

    assert_eq!(link.mtu.unwrap(), 9000);
    assert_eq!(link.class, datalink_class_t::DATALINK_CLASS_ETHERSTUB);
    assert_eq!(link.flags, DlAdmOpt::ACTIVE);
    assert_eq!(link.media, DlpiMediaType::Known(DlMediaType::Ether));
}
