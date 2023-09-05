// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

#![allow(dead_code)]

use crate::intr_pins::IntrPin;

pub struct AtaCtrl {
    channels: [Channel; 2],

    /// Interrupt pins
    ata0_pin: Option<Box<dyn IntrPin>>,
    ata1_pin: Option<Box<dyn IntrPin>>,

    /// Whether or not we should service guest commands
    paused: bool,

    pub ops: usize,
}

impl AtaCtrl {
    pub fn new() -> Self {
        Self {
            channels: [Channel::default(), Channel::default()],
            ata0_pin: None,
            ata1_pin: None,
            paused: false,
            ops: 0,
        }
    }

    pub fn attach_irq(
        &mut self,
        ata0_pin: Box<dyn IntrPin>,
        ata1_pin: Box<dyn IntrPin>,
    ) {
        self.ata0_pin = Some(ata0_pin);
        self.ata1_pin = Some(ata1_pin);
    }

    pub fn selected_device_type(&self, channel: usize) -> DeviceType {
        self.channels[channel].device().map_or(DeviceType::Ata, |d| d.kind)
    }

    pub fn read_command_block(&self, channel: usize, op: CommandRead) -> u16 {
        let val = match op {
            CommandRead::Status => 0x01,
            _ => 0x00,
        };

        println!("R: {} {:?} {:x}", channel, op, val);
        val
    }

    pub fn write_command_block(&self, channel: usize, op: CommandWrite) {
        println!("W: {} {:?}", channel, op);
    }

    pub fn read_control_block(&self, channel: usize, op: ControlRead) -> u16 {
        let val = match op {
            ControlRead::AltStatus => 0x01,
            _ => 0x00,
        };

        println!("R: {} {:?} {:x}", channel, op, val);
        val
    }

    pub fn write_control_block(&self, channel: usize, op: ControlWrite) {
        println!("W: {} {:?}", channel, op);
    }
}

#[derive(Default)]
struct Channel {
    devices: [Option<Device>; 2],
    selected_device: usize,
}

impl Channel {
    fn device(&self) -> Option<&Device> {
        self.devices[self.selected_device].as_ref()
    }
}

struct Device {
    kind: DeviceType,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeviceType {
    Ata,
    Atapi,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommandRead {
    Invalid(usize),
    Data,
    Error,
    SectorCount,
    InterruptReason,
    LbaLow,
    LbaMid,
    LbaHigh,
    ByteCountLow,
    ByteCountHigh,
    Device,
    DeviceSelect,
    Status,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommandWrite {
    Invalid(usize),
    Data(u16),
    Features(u8),
    SectorCount(u8),
    LbaLow(u8),
    LbaMid(u8),
    LbaHigh(u8),
    ByteCountLow(u8),
    ByteCountHigh(u8),
    Device(u8),
    DeviceSelect(u8),
    Command(u8),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ControlRead {
    Invalid(usize),
    AltStatus,
    DeviceAddress,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ControlWrite {
    Invalid(usize),
    DeviceControl(u8),
}
