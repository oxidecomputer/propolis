// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

#![allow(dead_code)]

use bitstruct::FromRaw;

use crate::hw::ata::bits::*;
use crate::hw::ata::geometry::*;
use crate::hw::ata::AtaError;

pub struct AtaDevice {
    sector: [u16; 256],
    sector_ptr: usize,
    registers: DeviceRegisters,
    capacity: Sectors,
    default_geometry: Geometry,
    active_geometry: Geometry,
    operation: Option<Operations>,
    irq: bool,
    log: slog::Logger,
    pub id: usize,
}

impl AtaDevice {
    pub fn create(
        log: slog::Logger,
        capacity: Sectors,
        default_geometry: Geometry,
    ) -> Self {
        // Execute Power on Reset, ATA/ATAPI-6, 9.1.
        Self {
            sector: [0u16; 256],
            sector_ptr: 0,
            registers: DeviceRegisters::default_ata(),
            capacity,
            default_geometry,
            active_geometry: default_geometry,
            operation: None,
            irq: false,
            log,
            // Id and path will be updated when the device is attached.
            id: 0,
        }
    }

    pub fn set_software_reset(&mut self, software_reset: bool) {
        if software_reset {
            self.registers.status.set_busy(true);
        } else if self.registers.device_control.software_reset()
            && !software_reset
        {
            slog::info!(self.log, "software reset");
            self.execute_device_diagnostics().unwrap();
        }

        self.registers.device_control.set_software_reset(software_reset);
    }

    /// Return the Status Register.
    pub fn status(&self) -> &StatusRegister {
        &self.registers.status
    }

    /// Return whether or not an interrupt is pending.
    pub fn interrupt(&self) -> bool {
        !self.registers.device_control.interrupt_enabled_n() && self.irq
    }

    fn execute_device_diagnostics(&mut self) -> Result<bool, AtaError> {
        // Set the diagnostics passed code. For a non-existent
        // device the channel will default to a value of 0x7f.
        self.registers.error = ErrorRegister::from_raw(0x1);

        // Set Status according to ATA/ATAPI-6, 9.10, D0ED3, p. 362.
        self.registers.status = StatusRegister::from_raw(1 << 6 | 1 << 4);

        self.set_signature();
        Ok(true)
    }

    fn complete_command_with_data_ready(
        &mut self,
        data_ready: bool,
    ) -> Result<bool, AtaError> {
        self.registers.error = ErrorRegister::from_raw(0x0);

        self.registers.status.set_error(false);
        self.registers.status.set_data_request(data_ready);
        self.registers.status.set_device_fault(false);
        self.registers.status.set_busy(false);

        Ok(true)
    }

    fn abort_command(&mut self, e: AtaError) -> Result<bool, AtaError> {
        self.registers.error.set_abort(true);
        self.registers.status.set_error(true);
        Err(e)
    }

    fn continue_operation(&mut self) -> Result<bool, AtaError> {
        match self.operation {
            Some(Operations::ReadSectors(sectors_remaining)) => {
                if sectors_remaining > 0 {
                    self.operation = Some(Operations::ReadSectors(sectors_remaining - 1));
                    self.sector = [0u16; 256];
                    self.sector_ptr = 0;

                    self.complete_command_with_data_ready(true)
                } else {
                    self.operation = None;
                    self.complete_command_with_data_ready(false)
                }
            }
            None =>
                self.complete_command_with_data_ready(false).and(Ok(false)),
        }
    }

    /// Execute the given command and return whether or not the Controller
    /// should raise an interrupt when done.
    pub fn execute_command(&mut self, c: Commands) -> Result<(), AtaError> {
        // if !self.registers.status.device_ready() {
        //     return Err(AtaError::DeviceNotReady);
        // }

        self.irq = self.irq
            || match c {
                // EXECUTE DEVICE DIAGNOSTICS, ATA/ATAPI-6, 8.11.
                Commands::ExecuteDeviceDiagnostics => {
                    self.execute_device_diagnostics()
                }

                Commands::Recalibrate => //panic!(),
                    self.complete_command_with_data_ready(false),

                // IDENTIFY DEVICE, ATA/ATAPI-6, 8.15.
                Commands::IdenfityDevice => {
                    self.set_identity();
                    self.complete_command_with_data_ready(true)
                }

                // INITIALIZE DEVICE PARAMETERS, ATA/ATAPI-4, 8.16.
                Commands::InitializeDeviceParameters => {
                    self.active_geometry.sectors =
                        self.registers.sector_count.read_current();
                    self.active_geometry.heads =
                        self.registers.device.heads() + 1;
                    self.active_geometry.compute_cylinders(self.capacity);

                    slog::info!(
                        self.log,
                        "initialize device parameters";
                        self.active_geometry);

                    self.complete_command_with_data_ready(false)
                }

                // SET FEATURES, ATA/ATAPI-6, 8.36.
                Commands::SetFeatures => match self.set_features() {
                    Ok(()) => self.complete_command_with_data_ready(false),
                    Err(e) => self.abort_command(e),
                },

                // SET MULTIPLE MODE, ATA/ATAPI-6, 8.39.
                Commands::SetMultipleMode => {
                    // TODO (arjen): Actually set the multiple mode instead of
                    // using a default of 1.
                    slog::info!(
                        self.log,
                        "set multiple mode";
                        "value" => self.registers.sector_count.read_current());

                    self.complete_command_with_data_ready(false)
                }

                Commands::ReadSectors => {
                    // TODO (arjen): Seek to address.
                    self.operation = Some(Operations::ReadSectors(self.registers.sector_count.read_current().into()));
                    self.continue_operation()
                }

                _ => self.abort_command(AtaError::UnsupportedCommand(c)),
            }?;

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
        self.sector_ptr = 0;

        // Set a serial number.
        copy_str("0123456789", &mut self.sector[10..20]);

        // Set a firmware version string.
        copy_str("v1", &mut self.sector[23..26]);

        // Set a model number string.
        copy_str("Propolis ATA-v1", &mut self.sector[27..46]);

        // Set device geometry and capacity.
        //
        // Set the default CHS translation.
        self.sector[1] = self.default_geometry.cylinders;
        self.sector[3] = self.default_geometry.heads as u16;
        self.sector[6] = self.default_geometry.sectors as u16;
        // Set the active CHS translation.
        self.sector[54] = self.active_geometry.cylinders;
        self.sector[55] = self.active_geometry.heads as u16;
        self.sector[56] = self.active_geometry.sectors as u16;
        // Set the addressable capacity using the active CHS translation.
        self.sector[57] = self.active_geometry.capacity().lba28_low();
        self.sector[58] = self.active_geometry.capacity().lba28_high();
        // Set the device capacity in LBA28.
        self.sector[60] = self.capacity.lba28_low();
        self.sector[61] = self.capacity.lba28_high();
        // Set the device capacity in LBA48.
        self.sector[100] = self.capacity.lba48_low();
        self.sector[101] = self.capacity.lba48_mid();
        self.sector[102] = self.capacity.lba48_high();

        // Set features and capabilities.
        self.sector[2] = 0x8c73; // No standby, IDENTITY is complete.
        self.sector[47] = 0x8001; // 1 sector per interrupt.
        self.sector[49] = 0x0200; // LBA supported, device manages standby timer values.
        self.sector[50] = 0x4000;
        self.sector[53] = 0x0006; // Words 70:64 and 88 are valid.
        self.sector[59] = 0x0001; // 1 sector per interrupt.
        self.sector[63] = 0x0000; // No Multiword DMA support.
        self.sector[64] = 0x0003; // PIO mode 4 support.
        self.sector[80] = 0x0030; // ATA-3, ATA/ATAPI-4 support.
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
    }

    fn set_features(&mut self) -> Result<(), AtaError> {
        match self.registers.features.read_current() {
            0x3 => {
                let arguments = self.registers.sector_count.read_current();
                let mode = (arguments & 0xf8) >> 3;
                let value = arguments & 0x7;

                match (mode, value) {
                    (0, 0) => Ok(
                        slog::info!(self.log, "set transfer mode"; "mode" => "PIO default"),
                    ),
                    (0, 1) => Ok(
                        slog::info!(self.log, "set transfer mode"; "mode" => "PIO default, disable IORDY"),
                    ),
                    (1, _) => Ok(
                        slog::info!(self.log, "set transfer mode"; "mode" => "PIO mode", "value" => value),
                    ),
                    (4, _) => Ok(
                        slog::info!(self.log, "set transfer mode"; "mode" => "Multiword DMA", "value" => value),
                    ),
                    (8, _) => Ok(
                        slog::info!(self.log, "set transfer mode"; "mode" => "Ultra DMA", "value" => value),
                    ),
                    (_, _) => Err(AtaError::FeatureNotSupported),
                }
            }
            _ => Err(AtaError::FeatureNotSupported),
        }
    }

    // Implement the ATA8-ATP, allowing for a channel to read/write the device
    // registers.

    pub fn read_data16(&mut self) -> Result<u16, AtaError> {
        let data = self.sector[self.sector_ptr];

        if self.registers.status.data_request() {
            if self.sector_ptr >= self.sector.len() - 1 {
                self.sector_ptr = 0;
                // Clear the DRQ bit to signal the host no more data is
                // available.
                self.registers.status.set_data_request(false);
                self.irq = self.continue_operation()?;
            } else {
                self.sector_ptr += 1;
            }
        }

        Ok(data)
    }

    pub fn read_data32(&mut self) -> Result<u32, AtaError> {
        let low = self.read_data16().map(u32::from)?;
        let high = self.read_data16().map(u32::from)?;

        Ok(high << 16 | low)
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

    pub fn write_register(
        &mut self,
        r: Registers,
        byte: u8,
    ) -> Result<(), AtaError> {
        match r {
            Registers::Features => self.registers.features.write(byte),
            Registers::SectorCount => self.registers.sector_count.write(byte),
            Registers::LbaLow => self.registers.lba_low.write(byte),
            Registers::LbaMid => self.registers.lba_mid.write(byte),
            Registers::LbaHigh => self.registers.lba_high.write(byte),
            Registers::Device => self.registers.device.0 = byte,
            Registers::DeviceControl => {
                self.registers.device_control.0 = byte;
                println!("     {} interrupt enabled: {}", self.id, !self.registers.device_control.interrupt_enabled_n());
            }
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Operations {
    ReadSectors(usize),
}

/// Copy the given str s with appropriate padding into the given word buffer
/// according to the byte order described in ATA/ATAPI-4, 8.12.8.
fn copy_str(s: &str, buffer: &mut [u16]) {
    use itertools::izip;
    use std::iter::*;

    // Create an iterator which chains/pads the bytes of the given str s with
    // additional ASCII spaces (20h).
    let padded_s = s.as_bytes().iter().cloned().chain(repeat(0x20));

    // Clone the padded string iterator into two iterators offset by one byte,
    // to allow iterating in chunks of two bytes.
    let padded_s_byte0 = padded_s.clone().step_by(2);
    let padded_s_byte1 = padded_s.clone().skip(1).step_by(2);

    // Copy from the two byte iterators to the buffer in BE order. See
    // ATA/ATAPI-4, 8.12.8.
    for (word, b0, b1) in izip!(buffer, padded_s_byte0, padded_s_byte1) {
        *word = u16::from_be_bytes([b0, b1]);
    }
}
