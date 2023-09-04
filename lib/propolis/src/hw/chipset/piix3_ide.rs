// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::sync::{Arc, Mutex};

use crate::common::RWOp;
use crate::hw::ata::AtaCtrl;
use crate::hw::chipset::Chipset;
use crate::hw::ibmpc;
use crate::hw::ids;
use crate::hw::pci;
use crate::inventory::Entity;
use crate::pio::{PioBus, PioFn};


pub struct Piix3IdeCtrl {
    /// IDE state
    ata_state: Mutex<AtaCtrl>,

    /// PCI device state
    pci_state: pci::DeviceState,
}

impl Piix3IdeCtrl {
    pub fn create() -> Arc<Self> {
        let ata_state = AtaCtrl::new();

        let pci_state = pci::Builder::new(pci::Ident {
            vendor_id: ids::pci::VENDOR_INTEL,
            device_id: ids::pci::PIIX3_IDE_DEV_ID,
            sub_vendor_id: ids::pci::VENDOR_OXIDE,
            sub_device_id: ids::pci::PIIX3_IDE_SUB_DEV_ID,
            class: pci::bits::CLASS_STORAGE,
            subclass: pci::bits::SUBCLASS_STORAGE_IDE,
            prog_if: pci::bits::PROGIF_IDE_LEGACY_MODE,
            ..Default::default()
        })
        .add_bar_io(pci::BarN::BAR0, ibmpc::LEN_ATA_IO * 2)
        .add_bar_io(pci::BarN::BAR1, ibmpc::LEN_ATA_CTRL * 2)
        .add_bar_io(pci::BarN::BAR2, ibmpc::LEN_ATA_IO * 2)
        .add_bar_io(pci::BarN::BAR3, ibmpc::LEN_ATA_CTRL * 2)
        .finish();

        Arc::new(Self { ata_state: Mutex::new(ata_state), pci_state })
    }

    pub fn attach_pio(self: &Arc<Self>, pio: &PioBus) {
        let this = Arc::clone(self);

        let piofn = Arc::new(move |port: u16, rwo: RWOp| this.pio_rw(port, rwo))
            as Arc<PioFn>;

        pio.register(ibmpc::PORT_ATA0_IO, ibmpc::LEN_ATA_IO, Arc::clone(&piofn))
            .unwrap();
        pio.register(ibmpc::PORT_ATA1_IO, ibmpc::LEN_ATA_IO, Arc::clone(&piofn))
            .unwrap();
        pio.register(
            ibmpc::PORT_ATA0_CTRL,
            ibmpc::LEN_ATA_CTRL,
            Arc::clone(&piofn),
        )
        .unwrap();
        pio.register(
            ibmpc::PORT_ATA1_CTRL,
            ibmpc::LEN_ATA_CTRL,
            Arc::clone(&piofn),
        )
        .unwrap();
    }

    pub fn attach_irq(self: &Arc<Self>, chipset: &dyn Chipset) {
        let mut ctrl = self.ata_state.lock().unwrap();
        ctrl.attach_irq(chipset.irq_pin(ibmpc::IRQ_ATA0).unwrap(), chipset.irq_pin(ibmpc::IRQ_ATA1).unwrap());
    }

    fn pio_rw(&self, port: u16, _rwo: RWOp) {
        println!("{}", port);
    }
}

impl pci::Device for Piix3IdeCtrl {
    fn device_state(&self) -> &pci::DeviceState {
        &self.pci_state
    }

    fn bar_rw(&self, bar: pci::BarN, mut rwo: RWOp) {
        println!("{:?}", bar); //, rwo);
    }
}

impl Entity for Piix3IdeCtrl {
    fn type_name(&self) -> &'static str {
        "pci-piix3-ide"
    }

    fn reset(&self) {
        self.pci_state.reset(self);
    }
}
