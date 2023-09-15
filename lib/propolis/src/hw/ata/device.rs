// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

#![allow(dead_code)]

use bitstruct::FromRaw;

use crate::hw::ata::bits::Commands::*;
use crate::hw::ata::bits::Registers::*;
use crate::hw::ata::bits::*;
use crate::hw::ata::geometry::*;
use crate::hw::ata::{AtaError, AtaError::*};

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

    fn execute_device_diagnostics(&mut self) -> Result<(), AtaError> {
        // Set the diagnostics passed code. For a non-existent
        // device the channel will default to a value of 0x7f.
        self.registers.error = ErrorRegister::from_raw(0x1);

        // Set Status according to ATA/ATAPI-6, 9.10, D0ED3, p. 362.
        self.registers.status = StatusRegister::from_raw(1 << 6 | 1 << 4);

        self.set_signature();
        Ok(())
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

    fn continue_operation(&mut self) -> Result<bool, AtaError> {
        match self.operation {
            Some(Operations::ReadSectors(sectors_remaining)) => {
                if sectors_remaining > 0 {
                    self.operation =
                        Some(Operations::ReadSectors(sectors_remaining - 1));
                    self.sector = [0u16; 256];
                    self.sector_ptr = 0;

                    self.complete_command_with_data_ready(true)
                } else {
                    self.operation = None;
                    self.complete_command_with_data_ready(false)
                }
            }
            None => self.complete_command_with_data_ready(false).and(Ok(false)),
        }
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

    // INITIALIZE DEVICE PARAMETERS, ATA/ATAPI-4, 8.16.
    fn initialize_device_parameters(&mut self) -> Result<(), AtaError> {
        self.active_geometry.sectors =
            self.registers.sector_count.read_current();
        self.active_geometry.heads = self.registers.device.heads() + 1;
        self.active_geometry.compute_cylinders(self.capacity);

        slog::info!(
            self.log,
            "initialize device parameters";
            self.active_geometry);

        Ok(())
    }

    // SET FEATURES, ATA/ATAPI-6, 8.36.
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
                    (_, _) => Err(FeatureNotSupported),
                }
            }
            _ => Err(FeatureNotSupported),
        }
    }

    // SET MULTIPLE MODE, ATA/ATAPI-6, 8.39.
    fn set_multiple_mode(&mut self) -> Result<(), AtaError> {
        // TODO (arjen): Actually set the multiple mode instead of using a
        // default of 1.
        slog::info!(
            self.log,
            "set multiple mode";
            "value" => self.registers.sector_count.read_current());

        Ok(())
    }

    fn check_not_busy(&self) -> Result<(), AtaError> {
        if !self.registers.status.busy() {
            Ok(())
        } else {
            Err(DeviceBusy)
        }
    }

    fn check_device_ready(&self) -> Result<(), AtaError> {
        if self.registers.status.device_ready() {
            Ok(())
        } else {
            Err(DeviceNotReady)
        }
    }

    fn abort_command(&mut self) {
        self.registers.error.set_abort(true);
        self.registers.status.set_error(true);
    }

    fn execute_non_data_command<F>(&mut self, op: F) -> Result<(), AtaError>
    where
        F: FnOnce(&mut Self) -> Result<(), AtaError>,
    {
        self.check_not_busy()?;
        self.check_device_ready()?;

        op(self)?;

        self.registers.status.set_busy(false);
        self.registers.status.set_device_ready(true);
        self.registers.status.set_data_request(false);
        self.registers.status.set_error(self.registers.error.0 != 0x0);
        self.irq = true;

        Ok(())
    }

    /// Execute the given command and return whether or not the Controller
    /// should raise an interrupt when done.
    pub fn execute_command(&mut self, value: u8) {
        if let Err(_e) = Commands::try_from(value).and_then(|c| {
            // Initiate one of the protocols based on the command.
            match c {
                DeviceReset => {
                    todo!()
                }
                Recalibrate => self.execute_non_data_command(|_| Ok(())),
                ExecuteDeviceDiagnostics => self.execute_device_diagnostics(),
                InitializeDeviceParameters => self.execute_non_data_command(
                    Self::initialize_device_parameters,
                ),
                SetFeatures => {
                    self.execute_non_data_command(Self::set_features)
                }
                SetMultipleMode => {
                    self.execute_non_data_command(Self::set_multiple_mode)
                }
                _ => Err(UnsupportedCommand(c)),
                // IDENTIFY DEVICE, ATA/ATAPI-6, 8.15.
                // Commands::IdenfityDevice => {
                //     self.set_identity();
                //     self.complete_command_with_data_ready(true)
                // }
                // Commands::ReadSectors => {
                //     // TODO (arjen): Seek to address.
                //     self.operation = Some(Operations::ReadSectors(self.registers.sector_count.read_current().into()));
                //     self.continue_operation()
                // }
            }
        }) {
            // This sets the high level Error.ABRT and Status.ERR bits. The
            // protocol is expected to set additional error bits as appropriate.
            self.abort_command();
            // TODO (arjen): Log error?
        }
    }

    pub fn read_data(&mut self) -> u16 {
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

        data
    }

    pub fn write_data(&mut self, _data: u16) {}

    pub fn read_register(&mut self, reg: Registers) -> u8 {
        match reg {
            Error => self.registers.error.0,
            SectorCount => self
                .registers
                .sector_count
                .read_u8(self.registers.device_control.high_order_byte()),
            LbaLow => self
                .registers
                .lba_low
                .read_u8(self.registers.device_control.high_order_byte()),
            LbaMid => self
                .registers
                .lba_mid
                .read_u8(self.registers.device_control.high_order_byte()),
            LbaHigh => self
                .registers
                .lba_high
                .read_u8(self.registers.device_control.high_order_byte()),
            Device => self.registers.device.0,
            Status => {
                // Reading Status clears the device interrupt.
                self.irq = false;
                self.registers.status.0
            }
            AltStatus => self.registers.status.0,
            _ => panic!(),
        }
    }

    pub fn write_register(&mut self, reg: Registers, value: u8) {
        match reg {
            Features => self.registers.features.write(value),
            SectorCount => self.registers.sector_count.write(value),
            LbaLow => self.registers.lba_low.write(value),
            LbaMid => self.registers.lba_mid.write(value),
            LbaHigh => self.registers.lba_high.write(value),
            Device => self.registers.device.0 = value,
            DeviceControl => {
                self.registers.device_control.0 = value;
                println!(
                    "     {} interrupt enabled: {}",
                    self.id,
                    !self.registers.device_control.interrupt_enabled_n()
                );
            }
            Command => self.execute_command(value),
            _ => panic!(),
        }

        // Per ATA/ATAPI-6, 6.20, clear the HOB when any of the Control Block
        // registers get written.
        if reg != DeviceControl {
            self.registers.device_control.set_high_order_byte(false);
        }
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
