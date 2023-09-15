// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::sync::{Arc, Mutex};

use crate::common::{RWOp, ReadOp, WriteOp};
use crate::hw::ata::{AtaController, Registers};
use crate::hw::chipset::Chipset;
use crate::hw::ibmpc;
use crate::hw::ids;
use crate::hw::pci;
use crate::inventory::Entity;
use crate::pio::{PioBus, PioFn};

pub struct Piix3IdeCtrl {
    /// IDE state
    ata_state: Arc<Mutex<AtaController>>,

    /// PCI device state
    pci_state: pci::DeviceState,
}

impl Piix3IdeCtrl {
    pub fn create(ata_state: Arc<Mutex<AtaController>>) -> Arc<Self> {
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

        Arc::new(Self { ata_state, pci_state })
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
        use ibmpc::*;
        use RWOp::*;

        match (port, rwo) {
            (PORT_ATA0_CMD, Read(op)) => self.read_command_block(0, op),
            (PORT_ATA1_CMD, Read(op)) => self.read_command_block(1, op),
            (PORT_ATA0_CMD, Write(op)) => self.write_command_block(0, op),
            (PORT_ATA1_CMD, Write(op)) => self.write_command_block(1, op),
            (PORT_ATA0_CTRL, Read(op)) => self.read_control_block(0, op),
            (PORT_ATA1_CTRL, Read(op)) => self.read_control_block(1, op),
            (PORT_ATA0_CTRL, Write(op)) => self.write_control_block(0, op),
            (PORT_ATA1_CTRL, Write(op)) => self.write_control_block(1, op),
            (_, _) => panic!(),
        }
    }

    fn read_command_block(&self, channel_id: usize, op: &mut ReadOp) {
        use Registers::*;

        let mut ata = self.ata_state.lock().unwrap();

        match op.offset() {
            0 if op.len() == 2 => op.write_u16(ata.read_data16(channel_id)),
            0 if op.len() == 4 => op.write_u32(ata.read_data32(channel_id)),
            1 => op.write_u8(ata.read_register(channel_id, Error)),
            2 => op.write_u8(ata.read_register(channel_id, SectorCount)),
            3 => op.write_u8(ata.read_register(channel_id, LbaLow)),
            4 => op.write_u8(ata.read_register(channel_id, LbaMid)),
            5 => op.write_u8(ata.read_register(channel_id, LbaHigh)),
            6 => op.write_u8(ata.read_register(channel_id, Device)),
            7 => op.write_u8(ata.read_register(channel_id, Status)),
            _ => panic!(),
        }
    }

    fn write_command_block(&self, channel_id: usize, op: &mut WriteOp) {
        use Registers::*;

        let mut ata = self.ata_state.lock().unwrap();

        match op.offset() {
            0 if op.len() == 2 => ata.write_data16(channel_id, op.read_u16()),
            0 if op.len() == 4 => ata.write_data32(channel_id, op.read_u32()),
            1 => ata.write_register(channel_id, Features, op.read_u8()),
            2 => ata.write_register(channel_id, SectorCount, op.read_u8()),
            3 => ata.write_register(channel_id, LbaLow, op.read_u8()),
            4 => ata.write_register(channel_id, LbaMid, op.read_u8()),
            5 => ata.write_register(channel_id, LbaHigh, op.read_u8()),
            6 => ata.write_register(channel_id, Device, op.read_u8()),
            7 => ata.write_register(channel_id, Command, op.read_u8()),
            _ => panic!(),
        }
    }

    fn read_control_block(&self, channel_id: usize, op: &mut ReadOp) {
        use Registers::*;

        let mut ata = self.ata_state.lock().unwrap();

        match op.offset() {
            0 => op.write_u8(ata.read_register(channel_id, AltStatus)),
            _ => panic!(),
        }
    }

    fn write_control_block(&self, channel_id: usize, op: &mut WriteOp) {
        use Registers::*;

        let mut ata = self.ata_state.lock().unwrap();

        match op.offset() {
            0 => ata.write_register(channel_id, DeviceControl, op.read_u8()),
            _ => panic!(),
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
