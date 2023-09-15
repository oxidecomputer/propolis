// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use crate::hw::ata::{bits::*, device::*};
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

    #[inline]
    pub fn read_data16(&mut self, channel_id: usize) -> u16 {
        self.channels[channel_id]
            .maybe_device()
            .map_or(u16::MAX, |device| device.read_data())
    }

    #[inline]
    pub fn read_data32(&mut self, channel_id: usize) -> u32 {
        self.channels[channel_id].maybe_device().map_or(u32::MAX, |device| {
            let low = u32::from(device.read_data());
            let high = u32::from(device.read_data());
            high << 16 | low
        })
    }

    #[inline]
    pub fn write_data16(&mut self, channel_id: usize, data: u16) {
        self.channels[channel_id]
            .maybe_device()
            .map(|device| device.write_data(data));
    }

    #[inline]
    pub fn write_data32(&mut self, channel_id: usize, data: u32) {
        self.channels[channel_id].maybe_device().map(|device| {
            device.write_data(data as u16);
            device.write_data((data >> 16) as u16);
        });
    }

    pub fn read_register(&mut self, channel_id: usize, reg: Registers) -> u8 {
        let value =
            if let Some(device) = self.channels[channel_id].maybe_device() {
                device.read_register(reg)
            } else if reg == Registers::Status || reg == Registers::AltStatus {
                0x7f
            } else {
                0xff
            };

        self.update_channel_interrupt(channel_id);

        println!(
            "R: {}:{} {:?} {:02x}",
            channel_id, self.channels[channel_id].device_selected, reg, value
        );

        value
    }

    pub fn write_register(
        &mut self,
        channel_id: usize,
        reg: Registers,
        value: u8,
    ) {
        // A physical ATA channel is a shared medium where both attached devices
        // receive all writes. The devices use the DEV bit in the Device
        // register to determine whether or not to process the write. When
        // emulating both the channel and the devices it should not be necessary
        // to apply all writes to both devices only to have one ignore most of
        // them, but there a few exceptions. This function handles these as
        // appropriate.
        //
        // For starters, the device selector should be updated when a Device
        // register gets written. Failing to do so would mean that the Device
        // register write does not get applied to the device now being
        // addressed, causing the HOB and head bits to get missed.
        if reg == Registers::Device {
            self.channels[channel_id].device_selected =
                DeviceRegister(value).device_select().into();
        }

        println!(
            "W: {}:{} {:?} {:02x}",
            channel_id, self.channels[channel_id].device_selected, reg, value
        );

        // If an EXECUTE DEVICE DIAGNOSTICS command is issued the command should
        // be broadcasted to all attached devices and the device select bit for
        // the channel should be cleared.
        if reg == Registers::Command
            && value == Commands::ExecuteDeviceDiagnostics
        {
            for maybe_device in self.channels[channel_id].devices.iter_mut() {
                // The EXECUTE DEVICE DIAGNOSTICS command is to be implemented
                // by every device and is expected to succeed. Simply unwrapping
                // may be good enough for now.
                maybe_device
                    .as_mut()
                    .map(|device| device.execute_command(value));
            }

            self.channels[channel_id].device_selected = 0;
        }

        // The DeviceControl.SRST bit should be processed by all attached
        // devices, triggering a soft reset when appropriate.
        if reg == Registers::DeviceControl {
            for maybe_device in self.channels[channel_id].devices.iter_mut() {
                maybe_device.as_mut().map(|device| {
                    device.set_software_reset(
                        DeviceControlRegister(value).software_reset(),
                    )
                });
            }
        }

        if let Some(device) = self.channels[channel_id].maybe_device() {
            device.write_register(reg, value)
        }

        self.update_channel_interrupt(channel_id);
    }

    /// Update the IRQ pin for the channel with given id depending on whether or
    /// not the selected device on the channel has a pending interrupt.
    fn update_channel_interrupt(&self, channel_id: usize) {
        let device_id = self.channels[channel_id].device_selected;
        let interrupt_pending = self.channels[channel_id].devices[device_id]
            .as_ref()
            .map_or(false, |device| device.interrupt_pending());

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

impl Channel {
    pub fn maybe_device(&mut self) -> Option<&mut AtaDevice> {
        self.devices[self.device_selected].as_mut()
    }
}
