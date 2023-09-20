// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

#![allow(dead_code)]

use std::sync::Arc;
use std::sync::Mutex;

use slog::{Record, Result, Serializer, KV};
use thiserror::Error;

use crate::accessors::MemAccessor;
use crate::block;
use crate::common::{RWOp, ReadOp, WriteOp};
use crate::hw::ibmpc::*;
use crate::hw::pci;
use crate::intr_pins::IntrPin;
use crate::pio::{PioBus, PioFn};

mod bits;
mod controller;
mod device;
mod geometry;
#[cfg(test)]
mod test;

use bits::{Commands, Registers};
use controller::AtaControllerState;
use device::AtaDeviceState;

#[derive(Debug, Error)]
pub enum AtaError {
    #[error("no device")]
    NoDevice,

    #[error("device is busy")]
    DeviceBusy,

    #[error("device not ready")]
    DeviceNotReady,

    #[error("no master boot record")]
    NoMasterBootRecord,

    #[error("unknown command code ({0})")]
    UnknownCommandCode(u8),

    #[error("unsupported command ({0})")]
    UnsupportedCommand(Commands),

    #[error("feature not supported ({0})")]
    FeatureNotSupported(u8),

    #[error("transfer mode not supported ({0}, {0})")]
    TransferModeNotSupported(u8, u8),
}

#[usdt::provider(provider = "propolis")]
mod probes {
    fn ata_cmd(cmd: u8) {}
}

pub struct PciAtaController {
    pci_state: Arc<pci::DeviceState>,
    ata_state: Arc<Mutex<AtaControllerState>>,
    block_devices: [[Arc<AtaBlockDevice>; 2]; 2],
}

pub struct AtaBlockDevice {
    pci_state: Arc<pci::DeviceState>,
    ata_state: Arc<Mutex<AtaControllerState>>,
    notifier: block::Notifier,
}

struct CompletionPayload {
    channel_id: u8,
    device_id: u8,
}

impl PciAtaController {
    pub fn new(pci_state: pci::DeviceState) -> Self {
        let pci_state = Arc::new(pci_state);
        let ata_state = Arc::new(Mutex::new(AtaControllerState::new()));

        Self {
            pci_state: pci_state.clone(),
            ata_state: ata_state.clone(),
            block_devices: [
                [
                    AtaBlockDevice::new(&pci_state, &ata_state),
                    AtaBlockDevice::new(&pci_state, &ata_state),
                ],
                [
                    AtaBlockDevice::new(&pci_state, &ata_state),
                    AtaBlockDevice::new(&pci_state, &ata_state),
                ],
            ],
        }
    }

    pub fn attach_pio(pio: &PioBus, piofn: &Arc<PioFn>) {
        pio.register(PORT_ATA0_CMD, LEN_ATA_CMD, piofn.clone()).unwrap();
        pio.register(PORT_ATA1_CMD, LEN_ATA_CMD, piofn.clone()).unwrap();
        pio.register(PORT_ATA0_CTRL, LEN_ATA_CTRL, piofn.clone()).unwrap();
        pio.register(PORT_ATA1_CTRL, LEN_ATA_CTRL, piofn.clone()).unwrap();
    }

    pub fn attach_irq(
        &self,
        ata0_pin: Box<dyn IntrPin>,
        ata1_pin: Box<dyn IntrPin>,
    ) {
        self.ata_state.lock().unwrap().attach_irq(ata0_pin, ata1_pin);
    }

    pub fn pio_rw(&self, port: u16, rwo: RWOp) {
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

impl pci::Device for PciAtaController {
    fn device_state(&self) -> &pci::DeviceState {
        &self.pci_state
    }

    fn bar_rw(&self, _bar: pci::BarN, _rwo: RWOp) {}
}

impl AtaBlockDevice {
    pub fn new(
        pci_state: &Arc<pci::DeviceState>,
        ata_state: &Arc<Mutex<AtaControllerState>>,
    ) -> Arc<Self> {
        Arc::new(Self {
            pci_state: pci_state.clone(),
            ata_state: ata_state.clone(),
            notifier: block::Notifier::new(),
        })
    }
}

impl block::Device for AtaBlockDevice {
    fn next(&self) -> Option<block::Request> {
        self.notifier.next_arming(|| None)
    }

    fn complete(
        &self,
        op: block::Operation,
        result: block::Result,
        payload: Box<block::BlockPayload>,
    ) {
    }

    fn accessor_mem(&self) -> MemAccessor {
        self.pci_state.acc_mem.child()
    }

    fn set_notifier(&self, val: Option<Box<block::NotifierFn>>) {
        self.notifier.set(val);
    }
}

impl KV for AtaError {
    fn serialize(
        &self,
        _rec: &Record,
        serializer: &mut dyn Serializer,
    ) -> Result {
        serializer.emit_str("error", &self.to_string())
    }
}
