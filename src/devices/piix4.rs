use std::sync::Arc;

use crate::pci;

pub struct Piix4HostBridge {}

impl Piix4HostBridge {
    pub fn new() -> Arc<pci::DeviceInst<Self>> {
        pci::Builder::new(pci::Ident {
            vendor_id: 0x8086,
            device_id: 0x1237,
            class: 0x06,
            subclass: 0x00,
            sub_vendor_id: 0,
            sub_device_id: 0,
        })
        .finish(Self {})
    }
}
impl pci::Device for Piix4HostBridge {}
