// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::sync::{Arc, Mutex};

use crate::common::RWOp;
use crate::hw::ata::{AtaCtrl, bits::Registers};
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
        .add_bar_io(pci::BarN::BAR0, ibmpc::LEN_ATA_CMD * 2)
        .add_bar_io(pci::BarN::BAR1, ibmpc::LEN_ATA_CTRL * 2)
        .add_bar_io(pci::BarN::BAR2, ibmpc::LEN_ATA_CMD * 2)
        .add_bar_io(pci::BarN::BAR3, ibmpc::LEN_ATA_CTRL * 2)
        .finish();

        Arc::new(Self { ata_state: Mutex::new(ata_state), pci_state })
    }

    pub fn attach_pio(self: &Arc<Self>, pio: &PioBus) {
        let this = Arc::clone(self);
        let piofn = Arc::new(move |port: u16, rwo: RWOp| this.pio_rw(port, rwo))
            as Arc<PioFn>;

        pio.register(
            ibmpc::PORT_ATA0_CMD,
            ibmpc::LEN_ATA_CMD,
            Arc::clone(&piofn),
        )
        .unwrap();
        pio.register(
            ibmpc::PORT_ATA1_CMD,
            ibmpc::LEN_ATA_CMD,
            Arc::clone(&piofn),
        )
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
        self.ata_state.lock().unwrap().attach_irq(
            chipset.irq_pin(ibmpc::IRQ_ATA0).unwrap(),
            chipset.irq_pin(ibmpc::IRQ_ATA1).unwrap(),
        );
    }

    fn pio_rw(&self, port: u16, rwo: RWOp) {
        let mut ata = self.ata_state.lock().unwrap();

        // Decode the ATA channel and request based on the port, operation
        // offset and selected device type. Then issue the request with the
        // ATA controller and respond to read operations.
        if port == ibmpc::PORT_ATA0_CMD || port == ibmpc::PORT_ATA1_CMD {
            let channel = if port == ibmpc::PORT_ATA0_CMD { 0 } else { 1 };

            match rwo {
                RWOp::Read(op) => {
                    let r =  match op.offset() {
                        0 => Registers::Data,
                        1 => Registers::Error,
                        2 => Registers::SectorCount,
                        3 => Registers::LbaLow,
                        4 => Registers::LbaMid,
                        5 => Registers::LbaHigh,
                        6 => Registers::Device,
                        7 => Registers::Status,
                        _ => panic!()
                    };

                    if op.len() == 1 {
                        op.write_u8(ata.read_register(channel, r) as u8)
                    } else {
                        op.write_u16(ata.read_register(channel, r))
                    }
                }
                RWOp::Write(op) => {
                    let (r, val) = match op.offset() {
                        0 => (Registers::Data, op.read_u16()),
                        1 => (Registers::Error, op.read_u8().into()),
                        2 => (Registers::SectorCount, op.read_u8().into()),
                        3 => (Registers::LbaLow, op.read_u8().into()),
                        4 => (Registers::LbaMid, op.read_u8().into()),
                        5 => (Registers::LbaHigh, op.read_u8().into()),
                        6 => (Registers::Device, op.read_u8().into()),
                        7 => (Registers::Command, op.read_u8().into()),
                        _ => panic!()
                    };

                    ata.write_register(channel, r, val)
                }
            }
        } else if port == ibmpc::PORT_ATA0_CTRL || port == ibmpc::PORT_ATA1_CTRL
        {
            let channel = if port == ibmpc::PORT_ATA0_CMD { 0 } else { 1 };

            match rwo {
                RWOp::Read(op) => {
                    let r = match op.offset() {
                        0 => Registers::AltStatus,
                        _ => panic!()
                    };

                    if op.len() == 1 {
                        op.write_u8(ata.read_register(channel, r) as u8)
                    } else {
                        op.write_u16(ata.read_register(channel, r))
                    }
                }
                RWOp::Write(op) => {
                    let (r, val) = match op.offset() {
                        0 => (Registers::DeviceControl, op.read_u8().into()),
                        _ => panic!(),
                    };

                    ata.write_register(channel, r, val)
                }
            }
        }
    }
}

impl pci::Device for Piix3IdeCtrl {
    fn device_state(&self) -> &pci::DeviceState {
        &self.pci_state
    }

    fn bar_rw(&self, bar: pci::BarN, mut _rwo: RWOp) {
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
