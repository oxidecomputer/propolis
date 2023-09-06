// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

#![allow(dead_code)]

use crate::hw::ata::bits::{Registers, Commands};

#[derive(Default)]
pub struct Device {
    error: Register,
    features: Register,
    sector_count: Register,
    lba_low: Register,
    lba_mid: Register,
    lba_high: Register,
    device: Register,
    status: Register,
    device_control: Register,
    id: usize,
}

impl Device {
    pub fn new(id: usize) -> Self {
        Self {
            id,
            ..Device::default()
        }
    }

    fn set_busy(&mut self, busy: bool) {}

    fn execute(&mut self, c: Commands) {
        match c {
            Commands::ExecuteDeviceDiagnostics => {
                // Set the diagnostics code. For a non-existent device the
                // channel will default to the correct value of 0x00.
                self.error.write(0x1, false);
                // Set the device signature, allowing the host to distinquish
                // between ATA and ATAPI devices.
                self.set_signature();
                self.set_busy(false);
            }
            _ => {
                // TODO (arjen): Log unsupported command.
            }
        }
    }

    /// Write the ATA device signature to the appropriate registers. See
    /// ATA/ATAPI-6, 9.12 for the values.
    fn set_signature(&mut self) {
        self.sector_count = Register(0x1);
        self.lba_low = Register(0x1);
        self.lba_mid = Register(0x0);
        self.lba_high = Register(0x0);
        self.device = Register(0x0);
    }

    // Implement the ATA8-ATP, allowing for a channel to read/write the device
    // registers.

    pub fn read_register(&mut self, r: Registers) -> u16 {
        match r {
            Registers::Data => 0x00,
            Registers::Error => self.error.read(false),
            Registers::SectorCount => self.sector_count.read(false),
            Registers::LbaLow => self.lba_low.read(false),
            Registers::LbaMid => self.lba_mid.read(false),
            Registers::LbaHigh => self.lba_high.read(false),
            Registers::Device => self.device.read(false),
            Registers::Status => self.status.read(false),
            Registers::AltStatus => self.status.read(false),
            _ => panic!(),
        }
    }

    pub fn write_register(&mut self, r: Registers, val: u16) {
        match r {
            Registers::Data => {},
            Registers::Command => match Commands::try_from(val as u8) {
                Ok(command) => self.execute(command),
                Err(_e) => {
                    // TODO (arjen): Log unknown command.
                },
            }
            Registers::Features => self.features.write(val, false),
            Registers::SectorCount => self.sector_count.write(val, false),
            Registers::LbaLow => self.lba_low.write(val, false),
            Registers::LbaMid => self.lba_mid.write(val, false),
            Registers::LbaHigh => self.lba_high.write(val, false),
            Registers::Device => self.device.write(val, false),
            Registers::DeviceControl => self.device_control.write(val, false),
            _ => panic!(),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Register(u32);

impl Register {
    #[inline]
    pub fn read(&mut self, windowed: bool) -> u16 {
        let this = self.0.to_le_bytes();
        let select_high_byte = this[3] != 0;

        if windowed {
            if select_high_byte {
                self.0 = u32::from_le_bytes([0, 0, this[1], this[0]]);
                this[1].into()
            } else {
                self.0 = u32::from_le_bytes([1, 0, this[1], this[0]]);
                this[0].into()
            }
        } else {
            self.0 as u16
        }
    }

    #[inline]
    pub fn write(&mut self, b: u16, windowed: bool) {
        let this = self.0.to_le_bytes();
        let select_high_byte = this[3] != 0;

        if windowed {
            if select_high_byte {
                self.0 = u32::from_le_bytes([0, 0, b as u8, this[0]]);
            } else {
                self.0 = u32::from_le_bytes([1, 0, this[1], b as u8]);
            }
        } else {
            self.0 = b as u32;
        }
    }
}
