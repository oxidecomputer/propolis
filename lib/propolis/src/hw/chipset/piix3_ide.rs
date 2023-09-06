// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::sync::{Arc, Mutex};

use crate::common::RWOp;
use crate::hw::ata::{
    AtaCtrl, CommandRead, CommandWrite, ControlRead, ControlWrite, DeviceType,
};
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
            let device_type =
                ata.selected_device_type(channel).unwrap_or(DeviceType::Ata);

            match rwo {
                RWOp::Read(op) => {
                    let response = ata.read_command_block(
                        channel,
                        match (op.offset(), device_type) {
                            (0, _) => CommandRead::Data,
                            (1, _) => CommandRead::Error,
                            (2, DeviceType::Ata) => CommandRead::SectorCount,
                            (2, DeviceType::Atapi) => {
                                CommandRead::InterruptReason
                            }
                            (3, DeviceType::Ata) => CommandRead::LbaLow,
                            (4, DeviceType::Ata) => CommandRead::LbaMid,
                            (4, DeviceType::Atapi) => CommandRead::ByteCountLow,
                            (5, DeviceType::Ata) => CommandRead::LbaHigh,
                            (5, DeviceType::Atapi) => {
                                CommandRead::ByteCountHigh
                            }
                            (6, _) => CommandRead::Device,
                            (7, _) => CommandRead::Status,
                            (_, _) => CommandRead::Invalid(op.offset()),
                        },
                    );

                    if op.len() == 1 {
                        op.write_u8(response as u8);
                    } else {
                        op.write_u16(response)
                    }
                }
                RWOp::Write(op) =>
                    let


                ata.write_command_block(
                    channel,
                    match (op.offset(), device_type) {
                        (0, _) => CommandWrite::Data(op.read_u16()),
                        (1, _) => CommandWrite::Features(op.read_u8()),
                        (2, DeviceType::Ata) => {
                            CommandWrite::SectorCount(op.read_u8())
                        }
                        (3, DeviceType::Ata) => {
                            CommandWrite::LbaLow(op.read_u8())
                        }
                        (4, DeviceType::Ata) => {
                            CommandWrite::LbaMid(op.read_u8())
                        }
                        (4, DeviceType::Atapi) => {
                            CommandWrite::ByteCountLow(op.read_u8())
                        }
                        (5, DeviceType::Ata) => {
                            CommandWrite::LbaHigh(op.read_u8())
                        }
                        (5, DeviceType::Atapi) => {
                            CommandWrite::ByteCountHigh(op.read_u8())
                        }
                        (6, _) => CommandWrite::Device(Registers::Device::from_u8(op.read_u8()))
                        (7, _) => CommandWrite::Command(op.read_u8()),
                        (_, _) => CommandWrite::Invalid(op.offset()),
                    },
                ),
            }
        } else if port == ibmpc::PORT_ATA0_CTRL || port == ibmpc::PORT_ATA1_CTRL
        {
            let channel = if port == ibmpc::PORT_ATA0_CTRL { 0 } else { 1 };
            let device_type =
                ata.selected_device_type(channel).unwrap_or(DeviceType::Ata);

            match rwo {
                RWOp::Read(op) => {
                    let response = ata.read_control_block(
                        channel,
                        match (op.offset(), device_type) {
                            (0, _) => ControlRead::AltStatus,
                            (1, DeviceType::Ata) => ControlRead::DeviceAddress,
                            (_, _) => ControlRead::Invalid(op.offset()),
                        },
                    );

                    if op.len() == 1 {
                        op.write_u8(response as u8);
                    } else {
                        op.write_u16(response)
                    }
                }
                RWOp::Write(op) => ata.write_control_block(
                    channel,
                    match (op.offset(), device_type) {
                        (0, _) => ControlWrite::DeviceControl(op.read_u8()),
                        (_, _) => ControlWrite::Invalid(op.offset()),
                    },
                ),
            }
        }
    }

    fn parse_pio_command_block_read(rwo: RWOp) {

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
