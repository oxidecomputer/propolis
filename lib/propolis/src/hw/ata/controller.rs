// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use crate::hw::ata::{bits::*, device::*, probes, AtaError};
use crate::intr_pins::IntrPin;

pub struct AtaController {
    channels: [Channel; 2],
    paused: bool,
}

impl AtaController {
    pub fn create() -> Self {
        Self {
            channels: [Channel::default(), Channel::default()],
            paused: false,
        }
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

    pub fn read_register(&mut self, channel_id: usize, r: Registers) -> u32 {
        let device_id = self.channels[channel_id].device_selected;
        let result = if let Some(device) =
            self.channels[channel_id].devices[device_id].as_mut()
        {
            match r {
                Registers::Data16 => device.read_data16().map(Into::into),
                Registers::Data32 => device.read_data32(),
                _ => device.read_register(r).map(Into::into),
            }
        } else {
            Err(AtaError::NoDevice)
        };

        self.update_channel_interrupt(channel_id);

        // Determine the data value returned to the caller.
        let value = match (result, r) {
            (Ok(val), _) => val,
            (_, _) => 0x7f,
        };

        println!(
            "R: {}:{} {:?} {:02x}",
            channel_id, self.channels[channel_id].device_selected, r, value
        );

        value
    }

    pub fn write_register(
        &mut self,
        channel_id: usize,
        r: Registers,
        word: u32,
    ) {
        let byte = word as u8;

        // A physical ATA channel is a shared medium where both attached devices
        // receive all writes. The devices use the DEV bit in the Device
        // register to determine whether or not to process the write. When
        // emulating both the channel and the devices it should not be necessary
        // to apply all writes to both devices only to have one ignore most of
        // them, but there a few exceptions. This function handles these as
        // appropriate.
        //
        // For starters, the selected device toggle should be updated when a
        // Device register gets written. Failing to do so would mean that the
        // Device register write does not get applied to the device now being
        // addressed, which means the HOB and head bits might get missed.
        if r == Registers::Device {
            self.channels[channel_id].device_selected =
                DeviceRegister(byte).device_select().into();
        }

        let device_id = self.channels[channel_id].device_selected;

        println!(
            "W: {}:{} {:?} {:02x}",
            channel_id, self.channels[channel_id].device_selected, r, word
        );

        // If an EXECUTE DEVICE DIAGNOSTICS command is issued the command should
        // be broadcasted to all attached devices and the device select bit for
        // the channel should be cleared.
        if r == Registers::Command
            && byte == Commands::ExecuteDeviceDiagnostics as u8
        {
            for maybe_device in self.channels[channel_id].devices.iter_mut() {
                // The EXECUTE DEVICE DIAGNOSTICS command is to be implemented
                // by every device and is expected to succeed. Simply unwrapping
                // may be good enough for now.
                maybe_device.as_mut().map(|device| {
                    device
                        .execute_command(Commands::ExecuteDeviceDiagnostics)
                        .unwrap()
                });
            }

            self.channels[channel_id].device_selected = 0;
        }

        // The DeviceControl.SRST bit should be processed by all attached
        // devices, triggering a soft reset when appropriate.
        if r == Registers::DeviceControl {
            for maybe_device in self.channels[channel_id].devices.iter_mut() {
                maybe_device.as_mut().map(|device| {
                    device.set_software_reset(
                        DeviceControlRegister(byte).software_reset(),
                    )
                });
            }
        }

        // All other writes can be issued to the currently selected device.
        let _result = self.channels[channel_id].devices[device_id]
            .as_mut()
            .ok_or(AtaError::NoDevice)
            .map(|device| -> Result<(), AtaError> {
                match r {
                    Registers::Data16 => device.write_data(word as u16),
                    Registers::Data32 => {
                        device.write_data(word as u16)?;
                        device.write_data((word >> 16) as u16)
                    }
                    Registers::Command => {
                        probes::ata_cmd!(|| byte);
                        device.execute_command(Commands::try_from(byte)?)
                    }
                    _ => device.write_register(r, byte),
                }
            });

        self.update_channel_interrupt(channel_id);
    }

    /// Update the IRQ pin for the channel with given id depending on whether or
    /// not the selected device on the channel has a pending interrupt.
    fn update_channel_interrupt(&self, channel_id: usize) {
        let device_id = self.channels[channel_id].device_selected;
        let interrupt_pending = self.channels[channel_id].devices[device_id]
            .as_ref()
            .map_or(false, |device| device.interrupt());

        if let Some(pin) = self.channels[channel_id].ata_pin.as_ref() {
            if interrupt_pending && !pin.is_asserted() {
                println!("     {} interrupt set", device_id);
            } else if !interrupt_pending && pin.is_asserted() {
                println!("     {} interrupt clear", device_id);
            }

            pin.set_state(interrupt_pending);

            // println!(
            //     "channel {} interrupt pending {}, pin {}",
            //     channel_id,
            //     interrupt_pending,
            //     pin.is_asserted()
            // );
        }
    }
}

#[derive(Default)]
struct Channel {
    devices: [Option<AtaDevice>; 2],
    device_selected: usize,
    ata_pin: Option<Box<dyn IntrPin>>,
}
