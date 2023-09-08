// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

#![allow(dead_code)]

use crate::intr_pins::IntrPin;

use thiserror::Error;

pub mod bits;
mod device;

use bits::{Commands, DeviceRegister, Registers};
pub use device::Device as AtaDevice;

#[derive(Debug, Error)]
pub enum AtaError {
    #[error("unknown command code ({0})")]
    UnknownCommandCode(u8),

    #[error("unsupported command ({0})")]
    UnsupportedCommand(Commands),
}

pub struct AtaCtrl {
    channels: [Channel; 2],

    /// Whether or not we should service guest commands
    paused: bool,
}

impl AtaCtrl {
    pub fn new() -> Self {
        Self { channels: [Channel::new(0), Channel::new(1)], paused: false }
    }

    pub fn attach_irq(
        &mut self,
        ata0_pin: Box<dyn IntrPin>,
        ata1_pin: Box<dyn IntrPin>,
    ) {
        self.channels[0].ata_pin = Some(ata0_pin);
        self.channels[1].ata_pin = Some(ata1_pin);
    }

    pub fn attach_device(&mut self, channel_id: usize, device_id: usize, device: AtaDevice) {
        self.channels[channel_id].devices[device_id] = Some(device.attach(channel_id, device_id));
    }

    #[inline]
    pub fn read_register(&mut self, channel_id: usize, r: Registers) -> u16 {
        self.channels[channel_id].read_register(r)
    }

    #[inline]
    pub fn write_register(
        &mut self,
        channel_id: usize,
        r: Registers,
        val: u16,
    ) {
        self.channels[channel_id].write_register(r, val)
    }
}

#[derive(Default)]
struct Channel {
    devices: [Option<AtaDevice>; 2],
    device_selected: usize,
    ata_pin: Option<Box<dyn IntrPin>>,
    id: usize,
}

impl Channel {
    pub fn new(id: usize) -> Self {
        Self { id, ..Channel::default() }
    }

    #[inline]
    fn device(&self) -> Option<&AtaDevice> {
        self.devices[self.device_selected].as_ref()
    }

    #[inline]
    fn device_mut(&mut self) -> Option<&mut AtaDevice> {
        self.devices[self.device_selected].as_mut()
    }

    fn update_interrupt_state(&self) {
        let any_interrupt = self
            .devices
            .iter()
            .flatten()
            .map(|device| device.interrupt())
            .any(|interrupt| interrupt);

        self.ata_pin.as_ref().map(|pin| pin.set_state(any_interrupt));
    }

    #[inline]
    pub fn read_register(&mut self, r: Registers) -> u16 {
        let result = self.device_mut().map_or(0x0, |d| match r {
            Registers::Data => d.read_data(),
            _ => d.read_register(r).into(),
        });

        self.update_interrupt_state();

        println!(
            "R: {}:{} {:?} {:x}",
            self.id, self.device_selected, r, result
        );

        result
    }

    #[inline]
    pub fn write_register(&mut self, r: Registers, word: u16) {
        println!("W: {}:{} {:?} {:x}", self.id, self.device_selected, r, word);

        let byte = word as u8;

        // Write to the device.
        self.device_mut().map(|d| match r {
            Registers::Data => d.write_data(word),
            Registers::Command => d.write_command(byte),
            _ => d.write_register(r, byte),
        });

        // If an EXECUTE DEVICE DIAGNOSTICS command is issued the device
        // select bit for the channel should be cleared.
        if r == Registers::Command
            && byte == Commands::ExecuteDeviceDiagnostics as u8
        {
            self.device_selected = 0;
        }

        // Update the selected device when the Device register gets written.
        if r == Registers::Device {
            self.device_selected = DeviceRegister(byte).device_select().into();
        }

        self.update_interrupt_state();
    }
}
