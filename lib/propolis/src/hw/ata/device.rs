// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

#![allow(dead_code)]

use crate::hw::ata::AtaError;
use crate::hw::ata::bits::*;

pub struct AtaDevice {
    sector: [u16; 256],
    sector_ptr: usize,
    registers: DeviceRegisters,
    irq: bool,
    pub id: usize,
}

impl AtaDevice {
    pub fn create() -> Self {
        // Execute Power on Reset, ATA/ATAPI-6, 9.1.
        Self {
            sector: [0u16; 256],
            sector_ptr: 0,
            registers: DeviceRegisters::default_ata(),
            irq: false,
            // Id will be updated when the device is attached.
            id: 0,
        }
    }

    /// Return the Status Register.
    pub fn status(&self) -> &StatusRegister {
        &self.registers.status
    }

    /// Return whether or not an interrupt is pending.
    pub fn interrupt(&self) -> bool {
        !self.registers.device_control.interrupt_enabled_n() && self.irq
    }

    fn complete_with_data_ready(&mut self, data: bool) -> bool {
        self.registers.error.0 = 0x0;

        self.registers.status.set_error(false);
        self.registers.status.set_data_request(data);
        self.registers.status.set_device_fault(false);
        self.registers.status.set_busy(false);

        data
    }

    /// Execute the given command and return whether or not the Controller
    /// should raise an interrupt when done.
    pub fn execute_command(&mut self, c: Commands) -> Result<(), AtaError> {
        if !self.registers.status.device_ready() {
            return Err(AtaError::DeviceNotReady);
        }

        self.irq = self.irq || match c {
            // EXECUTE DEVICE DIAGNOSTICS, ATA/ATAPI-6, 8.11.
            Commands::ExecuteDeviceDiagnostics => {
                // Set the diagnostics passed. code. For a non-existent device
                // the channel will default to a value of 0x0.
                self.registers.error.0 = 0x1;
                self.set_signature();
                // Set Status according to ATA/ATAPI-6, 9.10, D0ED3, p. 362.
                self.complete_with_data_ready(false)
            }

            // IDENTIFY DEVICE, see ATA/ATAPI-6, 8.15.
            Commands::IdenfityDevice => {
                self.sector_ptr = 0;
                self.set_identity();
                self.complete_with_data_ready(true)
            }

            Commands::SetFeatures => {
                self.set_features();
                false
            }

            _ => {
                self.registers.error.set_abort(true);
                self.registers.status.set_error(true);
                return Err(AtaError::UnsupportedCommand(c))
            }
        };

        Ok(())
    }

    /// Write the ATA device signature to the appropriate registers. See
    /// ATA/ATAPI-6, 9.12 for the values.
    fn set_signature(&mut self) {
        let ata_signature = DeviceRegisters::default_ata();

        self.registers.sector_count = ata_signature.sector_count;
        self.registers.lba_low = ata_signature.lba_low;
        self.registers.lba_mid = ata_signature.lba_mid;
        self.registers.lba_high = ata_signature.lba_high;
        self.registers.device.0 = 0x0;
    }

    /// Write the ATA/ATAPI-6 IDENTITY sector to the buffer. See ATA/ATAPI-6,
    /// 8.15.8 for details on the fields.
    fn set_identity(&mut self) {
        // The IDENTITY sector has a significant amount of empty/don't care
        // space. As such it's easier to simply clear the buffer and only fill
        // the words needed.
        self.sector = [0u16; 256];

        let n_sectors: u64 = 10 * 1024 * 1024 * 2;
        let n_sectors_lba28 = n_sectors as u32 & 0x0fffffff;
        let n_sectors_lba48 = n_sectors & 0x0000ffffffffffff;

        // Set a serial number.
        copy_str("0123456789", &mut self.sector[10..20]);

        // Set a firmware version string.
        copy_str("ata-v0.1", &mut self.sector[23..26]);

        // Set a model number string.
        copy_str("Propolis ATA HDD-v1", &mut self.sector[27..46]);

        self.sector[2] = 0x8c73; // No standby, IDENTITY is complete.
        self.sector[47] = 0x8001; // 1 sector per interrupt.
        self.sector[49] = 0x0200; // LBA supported, device manages standby timer values.
        self.sector[50] = 0x4000;
        self.sector[53] = 0x0006; // Words 70:64 and 88 are valid.
        self.sector[59] = 0x0001; // 1 sector per interrupt.
        self.sector[60] = (n_sectors_lba28 >> 16) as u16;
        self.sector[61] = (n_sectors_lba28 >> 0) as u16;
        self.sector[63] = 0x0000; // No Multiword DMA support.
        self.sector[64] = 0x0003; // PIO mode 4 support.

        // Set features supported.
        self.sector[80] = 0x0008; // ATA-3 support.
        self.sector[81] = 0x0000;
        self.sector[82] = 0x4000; // NOP support.
        self.sector[83] = 0x0000;
        self.sector[84] = 0x0000;
        self.sector[85] = 0x0000;
        self.sector[86] = 0x0000;
        self.sector[87] = 0x4000;
        self.sector[88] = 0x0000; // No Ultra DMA support.

        // Hardware reset result.
        self.sector[93] = 0x4101 | if self.id == 0 { 0x0006 } else { 0x0600 };

        self.sector[100] = (n_sectors_lba48 >> 32) as u16;
        self.sector[101] = (n_sectors_lba48 >> 16) as u16;
        self.sector[102] = (n_sectors_lba48 >> 0) as u16;
    }

    fn set_features(&mut self) {}

    // Implement the ATA8-ATP, allowing for a channel to read/write the device
    // registers.

    pub fn read_data(&mut self) -> Result<u16, AtaError> {
        let data = self.sector[self.sector_ptr];

        if self.registers.status.data_request() {
            if self.sector_ptr >= self.sector.len() - 1 {
                self.sector_ptr = 0;
                // Clear the DRQ bit to signal the host no more data is
                // available.
                self.registers.status.set_data_request(false);
            } else {
                self.sector_ptr += 1;
            }
        }

        Ok(data)
    }

    pub fn read_register(&mut self, r: Registers) -> Result<u8, AtaError> {
        Ok(match r {
            Registers::Error => self.registers.error.0,
            Registers::SectorCount => self
                .registers
                .sector_count
                .read_u8(self.registers.device_control.high_order_byte()),
            Registers::LbaLow => self
                .registers
                .lba_low
                .read_u8(self.registers.device_control.high_order_byte()),
            Registers::LbaMid => self
                .registers
                .lba_mid
                .read_u8(self.registers.device_control.high_order_byte()),
            Registers::LbaHigh => self
                .registers
                .lba_high
                .read_u8(self.registers.device_control.high_order_byte()),
            Registers::Device => self.registers.device.0,
            Registers::Status => {
                // Reading Status clears clears the device interrupt.
                self.irq = false;
                self.registers.status.0
            }
            Registers::AltStatus => self.registers.status.0,
            _ => panic!(),
        })
    }

    pub fn write_data(&mut self, _data: u16) -> Result<(), AtaError> {
        // Registers::Data => if self.registers.status.data_request() => {
        //     self.buffer[self.buffer_pointer] = val;
        //     self.buffer_pointer += 1;
        // }
        Ok(())
    }

    pub fn write_register(&mut self, r: Registers, byte: u8) -> Result<(), AtaError> {
        match r {
            Registers::Features => self.registers.features.write(byte),
            Registers::SectorCount => self.registers.sector_count.write(byte),
            Registers::LbaLow => self.registers.lba_low.write(byte),
            Registers::LbaMid => self.registers.lba_mid.write(byte),
            Registers::LbaHigh => self.registers.lba_high.write(byte),
            Registers::Device => self.registers.device.0 = byte,
            Registers::DeviceControl => self.registers.device_control.0 = byte,
            _ => panic!(),
        }

        // Per ATA/ATAPI-6, 6.20, clear the HOB when any of the Control Block
        // registers get written.
        if r != Registers::DeviceControl {
            self.registers.device_control.set_high_order_byte(false);
        }

        Ok(())
    }
}

struct DeviceRegisters {
    status: StatusRegister,
    error: ErrorRegister,
    device: DeviceRegister,
    device_control: DeviceControlRegister,
    features: FifoRegister,
    sector_count: FifoRegister,
    lba_low: FifoRegister,
    lba_mid: FifoRegister,
    lba_high: FifoRegister,
}

impl DeviceRegisters {
    fn default_ata() -> Self {
        Self {
            status: StatusRegister(0x40),
            error: ErrorRegister::default(),
            device: DeviceRegister::default(),
            device_control: DeviceControlRegister::default(),
            features: FifoRegister::default(),
            sector_count: FifoRegister::new(0x1),
            lba_low: FifoRegister::new(0x1),
            lba_mid: FifoRegister::default(),
            lba_high: FifoRegister::default(),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct FifoRegister([u8; 2]);

impl FifoRegister {
    #[inline]
    fn new(val: u8) -> Self {
        Self([val, 0])
    }

    #[inline]
    fn read_current(&self) -> u8 {
        self.0[0]
    }

    #[inline]
    fn read_previous(&self) -> u8 {
        self.0[1]
    }

    #[inline]
    fn read_u8(&self, previous: bool) -> u8 {
        if previous {
            self.read_previous()
        } else {
            self.read_current()
        }
    }

    #[inline]
    fn read_u16(&self) -> u16 {
        u16::from_le_bytes(self.0)
    }

    #[inline]
    fn write(&mut self, val: u8) {
        self.0[1] = self.0[0];
        self.0[0] = val;
    }
}

fn copy_str(s: &str, buffer: &mut [u16]) {
    use std::iter::*;

    // Iterate over the bytes of s, chaining null characters when s is
    // exhausted.
    let null_terminated_s = s.as_bytes().iter().cloned().chain(repeat(0u8));

    unsafe {
        let n_words = buffer.len();
        let (_, buffer_as_bytes, _) = buffer.align_to_mut::<u8>();
        assert!(buffer_as_bytes.len() == n_words * std::mem::size_of::<u16>());

        // Zip over the buffer words transmuted to bytes and the null terminated
        // str, while copying to the buffer. The result is a copy of s in the
        // buffer up to the lenght of the buffer, padded with null characters if
        // needed.
        for (b, s) in zip(buffer_as_bytes, null_terminated_s) {
            *b = s;
        }
    }
}
