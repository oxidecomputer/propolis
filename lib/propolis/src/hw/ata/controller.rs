// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use crate::intr_pins::IntrPin;
use crate::hw::ata::{AtaError, bits::*, device::*, probes};

pub struct AtaController {
    channels: [Channel; 2],
    paused: bool,
}

impl AtaController {
    pub fn create() -> Self {
        Self { channels: [Channel::default(), Channel::default()], paused: false }
    }

    pub fn attach_irq(
        &mut self,
        ata0_pin: Box<dyn IntrPin>,
        ata1_pin: Box<dyn IntrPin>,
    ) {
        self.channels[0].ata_pin = Some(ata0_pin);
        self.channels[1].ata_pin = Some(ata1_pin);
    }

    pub fn attach_device(
        &mut self,
        channel_id: usize,
        device_id: usize,
        mut device: AtaDevice,
    ) {
        device.id = device_id;
        self.channels[channel_id].devices[device_id] = Some(device);
    }

    pub fn read_register(
        &mut self,
        channel_id: usize,
        r: Registers,
    ) -> Result<u16, AtaError> {
        let device_id = self.channels[channel_id].device_selected;
        let result = self.channels[channel_id].devices[device_id].as_mut()
            .ok_or(AtaError::NoDevice)
            .map(|device| {
                match r {
                    Registers::Data => device.read_data(),
                    _ => device.read_register(r).map(Into::into),
                }
            })?;

        // Update the channel interrupt state.
        self.channels[channel_id]
            .ata_pin
            .as_ref()
            .map(|pin| pin.set_state(self.channel_interrupt(channel_id)));

        println!(
            "R: {}:{} {:?} {:?}",
            channel_id, self.channels[channel_id].device_selected, r, result
        );

        result
    }

    pub fn write_register(
        &mut self,
        channel_id: usize,
        r: Registers,
        word: u16,
    ) -> Result<(), AtaError> {
        println!(
            "W: {}:{} {:?} {:x}",
            channel_id, self.channels[channel_id].device_selected, r, word
        );

        let byte = word as u8;
        let device_id = self.channels[channel_id].device_selected;
        let result = self.channels[channel_id].devices[device_id].as_mut()
            .ok_or(AtaError::NoDevice)
            .map(|device| -> Result<(), AtaError> {
                match r {
                    Registers::Data => device.write_data(word),
                    Registers::Command => {
                        probes::ata_cmd!(|| byte);
                        device.execute_command(Commands::try_from(byte)?)
                    }
                    _ => device.write_register(r, byte),
                }
            })?;

        // If an EXECUTE DEVICE DIAGNOSTICS command is issued the device select
        // bit for the channel should be cleared.
        if r == Registers::Command
            && byte == Commands::ExecuteDeviceDiagnostics as u8
        {
            self.channels[channel_id].device_selected = 0;
        }

        // Update the selected device when a Device register gets written.
        if r == Registers::Device {
            self.channels[channel_id].device_selected =
                DeviceRegister(byte).device_select().into();
        }

        // Update the channel interrupt.
        self.channels[channel_id]
            .ata_pin
            .as_ref()
            .map(|pin| pin.set_state(self.channel_interrupt(channel_id)));

        result
    }

    /// Determine if the channel with the given id has pending interrupts.
    fn channel_interrupt(&self, id: usize) -> bool {
        self.channels[id]
            .devices
            .iter()
            .flatten()
            .map(|device| device.interrupt())
            .any(|interrupt| interrupt)
    }
}

#[derive(Default)]
struct Channel {
    devices: [Option<AtaDevice>; 2],
    device_selected: usize,
    ata_pin: Option<Box<dyn IntrPin>>,
}
