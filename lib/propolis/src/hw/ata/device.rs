// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

#![allow(dead_code)]

use std::convert::Infallible;
use std::sync::Mutex;

use bitstruct::FromRaw;
use unwrap_infallible::UnwrapInfallible;

use crate::block;
use crate::hw::ata::bits::Commands::*;
use crate::hw::ata::bits::Registers::*;
use crate::hw::ata::bits::*;
use crate::hw::ata::geometry::*;
use crate::hw::ata::{AtaError, AtaError::*};

pub struct AtaDevice {
    state: Mutex<State>,
}

impl AtaDevice {
    pub fn create(
        log: slog::Logger,
        capacity: Sectors,
        default_geometry: Geometry,
    ) -> Self {
        Self {
            state: Mutex::new(State::create(log, capacity, default_geometry)),
        }
    }

    pub fn interrupts_enabled(&self) -> bool {
        self.state.lock().unwrap().interrupts_enabled()
    }

    pub fn interrupt_pending(&self) -> bool {
        self.state.lock().unwrap().interrupt_pending()
    }

    pub fn set_software_reset(&self, software_reset: bool) {
        self.state.lock().unwrap().set_software_reset(software_reset)
    }

    pub fn read_data(&self) -> u16 {
        self.state.lock().unwrap().read_data()
    }

    pub fn write_data(&self, data: u16) {
        self.state.lock().unwrap().write_data(data)
    }

    pub fn read_register(&self, reg: Registers) -> u8 {
        self.state.lock().unwrap().read_register(reg)
    }

    pub fn write_register(&self, reg: Registers, value: u8) {
        self.state.lock().unwrap().write_register(reg, value)
    }

    pub fn status(&self) -> StatusRegister {
        StatusRegister(self.state.lock().unwrap().read_register(Registers::Status))
    }

    pub fn alt_status(&self) -> StatusRegister {
        StatusRegister(self.state.lock().unwrap().read_register(Registers::AltStatus))
    }

    pub fn error(&self) -> ErrorRegister {
        ErrorRegister(self.state.lock().unwrap().read_register(Registers::Error))
    }
}

// impl block::Device for AtaDevice {
//     fn next(&self) -> Option<Request> {
//         None
//     }
//     fn complete(&self, op: Operation, res: Result, payload: Box<BlockPayload>) {}
//     fn accessor_mem(&self) -> MemAccessor {

//     }
//     fn set_notifier(&self, f: Option<Box<NotifierFn>>) {}
// }

pub struct State {
    sector: [u16; 256],
    registers: DeviceRegisters,
    capacity: Sectors,
    default_geometry: Geometry,
    active_geometry: Geometry,
    pio_mode: u8,
    dma_mode: u8,
    operation: Option<Operations>,
    irq: bool,
    //info: block::DeviceInfo,
    //notifier: block::Notifier,
    log: slog::Logger,
    pub id: usize,
}

impl State {
    pub fn create(
        log: slog::Logger,
        capacity: Sectors,
        default_geometry: Geometry,
    ) -> Self {
        // Execute Power on Reset, ATA/ATAPI-6, 9.1.
        Self {
            sector: [0u16; 256],
            registers: DeviceRegisters::default_ata(),
            capacity,
            default_geometry,
            active_geometry: default_geometry,
            pio_mode: 0,
            dma_mode: 0,
            operation: None,
            irq: false,
            //info: block::DeviceInfo::default(),
            //notifier: block::Notifier::default(),
            log,
            // Id and path will be updated when the device is attached.
            id: 0,
        }
    }

    #[inline]
    pub fn interrupts_enabled(&self) -> bool {
        !self.registers.device_control.interrupt_enabled_n()
    }

    /// Return whether or not an interrupt is pending for this device.
    #[inline]
    pub fn interrupt_pending(&self) -> bool {
        self.interrupts_enabled() && self.irq
    }

    pub fn set_software_reset(&mut self, software_reset: bool) {
        if software_reset {
            self.registers.status.set_busy(true);
        } else if self.registers.device_control.software_reset()
            && !software_reset
        {
            slog::info!(self.log, "software reset");
            self.execute_device_diagnostics().unwrap_infallible();
        }

        self.registers.device_control.set_software_reset(software_reset);
    }

    /// Read a word of data from the Data port.
    pub fn read_data(&mut self) -> u16 {
        use Operations::*;

        match self.operation {
            // Make sure a DataIn operation is in progress and Status.DRQ is
            // set.
            Some(DataIn(data_in)) if self.registers.status.data_request() => {
                let data = self.sector[data_in.sector_ptr];

                // Update the operation state.
                if data_in.done() {
                    self.operation = None;
                    self.registers.status.set_data_request(false);
                } else if data_in.last_sector_word() {
                    self.operation = Some(DataIn(data_in.next_sector()));

                    // TODO (arjen): Handle multiple sectors per interrupt.
                    self.irq = true;
                } else {
                    self.operation = Some(DataIn(data_in.next_word()));
                }

                data
            }

            // Return a safe default if no DataIn operation in progress.
            _ => 0x0,
        }
    }

    /// Write the given word of data to the Data port.
    pub fn write_data(&mut self, _data: u16) {}

    /// Read one of the Command Block or Control Block registers.
    pub fn read_register(&mut self, reg: Registers) -> u8 {
        let hob = self.registers.device_control.high_order_byte();

        match reg {
            Error => self.registers.error.0,
            SectorCount => self.registers.sector_count.read_u8(hob),
            LbaLow => self.registers.lba_low.read_u8(hob),
            LbaMid => self.registers.lba_mid.read_u8(hob),
            LbaHigh => self.registers.lba_high.read_u8(hob),
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

    /// Write to one of the Command Block or Control Block registers.
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

        // Per ATA/ATAPI-6, 6.20, clear DeviceControl.HOB when any of the
        // Control Block registers get written.
        if reg != DeviceControl {
            self.registers.device_control.set_high_order_byte(false);
        }
    }

    /// Execute the given command.
    fn execute_command(&mut self, value: u8) {
        if let Err(e) = Commands::try_from(value).and_then(|command| {
            // Initiate one of the protocols based on the command.
            match command {
                DeviceReset => {
                    todo!()
                }
                Recalibrate => self.execute_non_data_command(|_| Ok(())),
                ExecuteDeviceDiagnostics => {
                    Ok(self.execute_device_diagnostics().unwrap_infallible())
                }
                IdenfityDevice => {
                    self.execute_pio_data_in_command(Self::set_identity)
                }
                InitializeDeviceParameters => self.execute_non_data_command(
                    Self::initialize_device_parameters,
                ),
                SetFeatures => {
                    self.execute_non_data_command(Self::set_features)
                }
                SetMultipleMode => {
                    self.execute_non_data_command(Self::set_multiple_mode)
                }
                ReadSectors => {
                    self.execute_pio_data_in_command(Self::seek_and_read_sector)
                }
                _ => Err(UnsupportedCommand(command)),
            }
        }) {
            // This sets the high level Error.ABRT and Status.ERR bits. The
            // protocol is expected to set additional error bits as appropriate.
            self.abort_command();

            slog::warn!(self.log, "command aborted"; e);
        }
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

    fn execute_pio_data_in_command<F>(&mut self, op: F) -> Result<(), AtaError>
    where
        F: FnOnce(&mut Self) -> Result<DataInOut, AtaError>,
    {
        self.check_not_busy()?;
        self.check_device_ready()?;

        self.operation = Some(Operations::DataIn(op(self)?));

        self.registers.status.set_busy(false);
        self.registers.status.set_device_ready(true);
        self.registers.status.set_data_request(true);
        self.registers.status.set_error(self.registers.error.0 != 0x0);
        self.irq = true;

        Ok(())
    }

    fn execute_device_diagnostics(&mut self) -> Result<(), Infallible> {
        // Set the diagnostics passed code. For a non-existent
        // device the channel will default to a value of 0x7f.
        self.registers.error = ErrorRegister::from_raw(0x1);

        // Set Status according to ATA/ATAPI-6, 9.10, D0ED3, p. 362.
        self.registers.status = StatusRegister::from_raw(1 << 6 | 1 << 4);

        self.set_signature();
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
    fn set_identity(&mut self) -> Result<DataInOut, AtaError> {
        // The IDENTITY sector has a significant amount of empty/don't care
        // space. As such it's easier to simply clear the buffer and only fill
        // the words needed.
        self.sector = [0u16; 256];

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
        self.sector[51] = (self.pio_mode as u16) << 8;
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

        // Set the PIO Data In protocol state with the sector pointer at 0 and
        // no remaining sectors.
        Ok(DataInOut::default())
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
            self.active_geometry,
            "capacity" => self.active_geometry.capacity());

        Ok(())
    }

    // SET FEATURES, ATA/ATAPI-6, 8.36.
    fn set_features(&mut self) -> Result<(), AtaError> {
        match self.registers.features.read_current() {
            0x3 => {
                let arguments = self.registers.sector_count.read_current();
                let transfer_type = (arguments & 0xf8) >> 3;
                let value = arguments & 0x7;
                let result = match (transfer_type, value) {
                    (0, 0) => Ok(self.pio_mode = 0),
                    (0, 1) => Ok(self.pio_mode = 0),
                    (1, value) => Ok(self.pio_mode = value),
                    (8, value) => Ok(self.dma_mode = value),
                    (_, _) => {
                        Err(TransferModeNotSupported(transfer_type, value))
                    }
                };

                if result.is_ok() {
                    slog::info!(
                        self.log,
                        "set transfer mode";
                        "type" => match transfer_type {
                            0 | 1 => "PIO",
                            8 => "UltraDMA",
                            _ => panic!(),
                        },
                        "mode" => value);
                }

                result
            }
            code => Err(FeatureNotSupported(code)),
        }
    }

    // SET MULTIPLE MODE, ATA/ATAPI-6, 8.39.
    fn set_multiple_mode(&mut self) -> Result<(), AtaError> {
        // TODO (arjen): Actually set the multiple mode instead of using a
        // default of 1.
        slog::info!(
            self.log,
            "set multiple mode";
            "sectors" => self.registers.sector_count.read_current());

        Ok(())
    }

    fn seek_and_read_sector(&mut self) -> Result<DataInOut, AtaError> {
        let address = if !self.registers.device.lba_addressing() {
            let chs = self.cylinder_head_sector_address();
            // TODO (arjen): Test if address valid.
            chs.logical_block_address(&self.active_geometry)
        } else {
            let lba = self.logical_block_address28();
            // TODO (arjen): Test if address valid.
            lba
        };

        Ok(DataInOut::default())
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

    #[inline]
    fn logical_block_address28(&self) -> LogicalBlockAddress {
        LogicalBlockAddress::new(
            u64::from(self.registers.lba_low.read_current())
                | u64::from(self.registers.lba_mid.read_current()) << 8
                | u64::from(self.registers.lba_high.read_current()) << 16
                | u64::from(self.registers.device.heads()) << 24,
        )
    }

    #[inline]
    fn logical_block_address48(&self) -> LogicalBlockAddress {
        todo!()
    }

    #[inline]
    fn cylinder_head_sector_address(&self) -> CylinderHeadSectorAddress {
        // Determine the logical address from the CHS address.
        let cylinder = u16::from(self.registers.lba_mid.read_current()) << 8
            | u16::from(self.registers.lba_high.read_current());
        CylinderHeadSectorAddress {
            cylinder,
            head: self.registers.device.heads(),
            sector: self.registers.lba_low.read_current(),
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

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct DataInOut {
    sector_address: usize,
    sector_ptr: usize,
    sectors_remaining: usize,
}

impl DataInOut {
    #[inline]
    pub fn last_sector_word(&self) -> bool {
        self.sector_ptr
            >= crate::hw::ata::BLOCK_SIZE / std::mem::size_of::<u16>() - 1
    }

    #[inline]
    pub fn done(&self) -> bool {
        self.last_sector_word() && self.sectors_remaining == 0
    }

    #[inline]
    pub fn next_word(&self) -> Self {
        let sector_ptr = self.sector_ptr + 1;
        Self { sector_ptr, ..*self }
    }

    #[inline]
    pub fn next_sector(&self) -> Self {
        let sector_address = self.sector_address + 1;
        let sectors_remaining = self.sectors_remaining - 1;
        Self { sector_address, sectors_remaining, ..*self }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Operations {
    DataIn(DataInOut),
    DataOut(DataInOut),
}

impl Operations {
    pub fn is_data_in(&self) -> bool {
        match self {
            Self::DataIn(_) => true,
            _ => false,
        }
    }
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
