// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

#![allow(dead_code)]

use crate::intr_pins::IntrPin;

use thiserror::Error;

pub mod bits;
mod device;

use bits::{Registers, Commands};
use device::Device;

#[derive(Debug, Error)]
pub enum AtaError {
    #[error("unknown command code ({0})")]
    UnknownCommandCode(u8),

    #[error("unsupported command ({0})")]
    UnsupportedCommand(Commands),
}

pub struct AtaCtrl {
    channels: [Channel; 2],

    /// Interrupt pins
    ata0_pin: Option<Box<dyn IntrPin>>,
    ata1_pin: Option<Box<dyn IntrPin>>,

    /// Whether or not we should service guest commands
    paused: bool,
}

impl AtaCtrl {
    pub fn new() -> Self {
        Self {
            channels: [Channel::new(0), Channel::new(1)],
            ata0_pin: None,
            ata1_pin: None,
            paused: false,
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

    #[inline]
    pub fn read_register(&mut self, channel: usize, r: Registers) -> u16 {
        self.channels[channel].read_register(r)
    }

    #[inline]
    pub fn write_register(&mut self, channel: usize, r: Registers, val: u16) {
        self.channels[channel].write_register(r, val)
    }
}

#[derive(Default)]
struct Channel {
    devices: [Option<Device>; 2],
    device_select: bool,
    id: usize,
}

impl Channel {
    pub fn new(id: usize) -> Self {
        Self {
            id,
            ..Channel::default()
        }
    }

    #[inline]
    fn device(&self) -> Option<&Device> {
        if self.device_select {
            self.devices[1].as_ref()
        } else {
            self.devices[0].as_ref()
        }
    }

    #[inline]
    fn device_mut(&mut self) -> Option<&mut Device> {
        if self.device_select {
            self.devices[1].as_mut()
        } else {
            self.devices[0].as_mut()
        }
    }

    #[inline]
    pub fn read_register(&mut self, r: Registers) -> u16 {
        let result = self.device_mut().map_or(0, |d| d.read_register(r));
        let device_id = if self.device_select { 1 } else { 0 };

        println!("R: {}:{} {:?} {:x}", self.id, device_id, r, result);

        if r == Registers::Device {
            // Make sure device_select is reflected as DEV bit in the Device
            // register.
            if self.device_select { result | 0x10u16 } else { result & !0u16 }
        } else {
            result
        }
    }

    #[inline]
    pub fn write_register(&mut self, r: Registers, val: u16) {
        let device_id = if self.device_select { 1 } else { 0 };

        println!("W: {}:{} {:?} {:x}", self.id, device_id, r, val);

        self.device_mut().map(|d| d.write_register(r, val));

        // If an EXECUTE DEVICE DIAGNOSTICS command is issued the device select
        // bit for the channel should be cleared.
        if r == Registers::Command && val == 0x90 {
            self.device_select = false;
        }
    }
}
