// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

#![allow(dead_code)]

use crate::intr_pins::IntrPin;

use thiserror::Error;

mod bits;

use bits::*;

#[derive(Debug, Error)]
pub enum AtaError {
    #[error("the command block read is invalid for the given device type")]
    InvalidCommandBlockRead(usize),

    #[error("the command block write is invalid for the given device type")]
    InvalidCommandBlockWrite(usize),

    #[error("the control block read is invalid for the given device type")]
    InvalidControlBlockRead(usize),

    #[error("the control block write is invalid for the given device type")]
    InvalidControlBlockWrite(usize),
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
            channels: [Channel::default(), Channel::default()],
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

    pub fn selected_device_type(&self, channel: usize) -> Option<DeviceType> {
        self.channels[channel].device().map(|d| d.kind)
    }

    pub fn read_command_block(&self, channel_select: usize, op: CommandRead) -> u16 {
        let channel = &self.channels[channel_select];
        let device = channel.device();

        let val = match op {
            // Read from the buffer.
            CommandRead::Data => 0,


            Some(device) =>
            None =>
        }

        let val = match op {
            CommandRead::Device => {
                self.channels[channel].registers.device
                    | DeviceRegister::default().0
            }
            CommandRead::Status => self.channels[channel].registers.status.0,
            _ => 0x00,
        };

        println!("R: {} {:?} {:x}", channel, op, val);
        val.into()
    }

    pub fn write_command_block(&mut self, channel: usize, op: CommandWrite) {
        // Parse and validate the write and execute any side-effects.
        match op {
            CommandWrite::Device(val) => {
                self.channels[channel].registers.device =
                    val | DeviceRegister::default().0
            }
            CommandWrite::Command(opcode) => match Commands::try_from(opcode) {
                Ok(command) => self.execute_command(channel, command),
                Err(AtaError::InvalidValue) => {
                    println!("W: {} {:?}", channel, op)
                }
            },
            _ => println!("W: {} {:?}", channel, op),
        }
    }

    pub fn read_control_block(&self, channel: usize, op: ControlRead) -> u16 {
        let val = match op {
            ControlRead::AltStatus => self.channels[channel].registers.status.0,
            _ => 0x00,
        };

        println!("R: {} {:?} {:x}", channel, op, val);
        val.into()
    }

    pub fn write_control_block(&self, channel: usize, op: ControlWrite) {
        println!("W: {} {:?}", channel, op);
    }

    fn execute_command(&mut self, channel_select: usize, command: Commands) {
        let channel = &self.channels[channel_select];
        let device = channel.device();

        println!("W: {} {} {:?}", channel, if channel.device_select { 1 } else { 0 }, command);

        match command {
            // EXECUTE DEVICE DIAGNOSTIC as per ATA-ATAPI-6, 8.11.
            //
            // Note that on real hardware this protocol executed on both
            // attached devices there are there are some differences between the
            // diagnostic codes. But since this emulation is simpler this gets
            // reduced to simply setting the first bit if a device is present.
            Commands::ExecuteDeviceDiagnostics => {
                channel.device().map(|d| {
                    d.registers.set_type_signature(DeviceType::Ata);
                })

                // Clear the device select bit.
                channel.device_select = false;
            }
        }
    }
}

#[derive(Default)]
struct Channel {
    devices: [Option<Device>; 2],
    device_select: bool,
}

impl Channel {
    #[inline]
    fn device(&self) -> Option<&Device> {
        if self.device_select {
            self.devices[1]
        } else {
            self.devices[0]
        }.as_ref()
    }

    #[inline]
    fn device_mut(&self) -> Option<&mut Device> {
        if self.device_select {
            self.devices[1]
        } else {
            self.devices[0]
        }.as_mut()
    }

    fn read_command_register(&self, op: CommandRead) -> u8

    fn error(&self) -> u8 {
        self.device().registers.error;
    }

    self.sector_count = 0x01;
    self.lba_low = 0x01;
    self.lba_mid = 0x00;
    self.lba_high = 0x00;
    self.device = 0x00;
}

struct Device {
    kind: DeviceType,
    registers: DeviceRegisters;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeviceType {
    Ata,
    Atapi,
}

#[derive(Default)]
struct DeviceRegisters {
    error: u8,
    sector_count: u8,
    lba_low: u8,
    lba_mid: u8,
    lba_high: u8,
    device: u8,
    status: u8,
}

impl DeviceRegisters {
    pub fn set_type_signature(&mut self, kind: DeviceType) {
        self.error = 0x01
        self.sector_count = 0x01;
        self.lba_low = 0x01;
        self.lba_mid = 0x00;
        self.lba_high = 0x00;
        self.device = 0x00;
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommandRead {
    Invalid(usize),
    Data,
    Error,
    SectorCount,
    InterruptReason,
    LbaLow,
    LbaMid,
    LbaHigh,
    ByteCountLow,
    ByteCountHigh,
    Device,
    Status,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommandWrite {
    Invalid(usize),
    Data(u16),
    Features(u8),
    SectorCount(u8),
    LbaLow(u8),
    LbaMid(u8),
    LbaHigh(u8),
    ByteCountLow(u8),
    ByteCountHigh(u8),
    Device(u8),
    DeviceSelect(u8),
    Command(u8),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ControlRead {
    Invalid(usize),
    AltStatus,
    DeviceAddress,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ControlWrite {
    Invalid(usize),
    DeviceControl(u8),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Commands {
    Nop = 0x00,
    ExecuteDeviceDiagnostics = 0x90,
    IdenfityDevice = 0xec,
    IdentifyPacketDevice = 0xa1,
    Idle = 0xe3,
    IdleImmediate = 0xe1,
    Packet = 0xa0,
    ReadSectors = 0x20,
    ReadSectorsWithoutRetry = 0x21,
    ReadSectorsExt = 0x24,
    ReadDma = 0xc8,
    ReadDmaWithoutRetry = 0xc9,
    ReadDmaExt = 0x25,
    ReadDmaQueued = 0xc7,
    ReadDmaQueuedExt = 0x26,
    WriteDma = 0xca,
    WriteDmaWithoutRetry = 0xcb,
    WriteDmaExt = 0x35,
    WriteDmaQueued = 0xcc,
    WriteDmaQueuedExt = 0x36,
    CacheFlush = 0xe7,
    CasheFlushExt = 0xea,
}

impl TryFrom<u8> for Commands {
    type Error = AtaError;

    fn try_from(opcode: u8) -> Result<Self, Self::Error> {
        match opcode {
            0x00 => Ok(Commands::Nop),
            0x90 => Ok(Commands::ExecuteDeviceDiagnostics),
            0xec => Ok(Commands::IdenfityDevice),
            0xa1 => Ok(Commands::IdentifyPacketDevice),
            0xe3 => Ok(Commands::Idle),
            0xe1 => Ok(Commands::IdleImmediate),
            0xa0 => Ok(Commands::Packet),
            0x20 => Ok(Commands::ReadSectors),
            0x21 => Ok(Commands::ReadSectorsWithoutRetry),
            0x24 => Ok(Commands::ReadSectorsExt),
            0xc8 => Ok(Commands::ReadDma),
            0xc9 => Ok(Commands::ReadDmaWithoutRetry),
            0x25 => Ok(Commands::ReadDmaExt),
            0xc7 => Ok(Commands::ReadDmaQueued),
            0x26 => Ok(Commands::ReadDmaQueuedExt),
            0xca => Ok(Commands::WriteDma),
            0xcb => Ok(Commands::WriteDmaWithoutRetry),
            0x35 => Ok(Commands::WriteDmaExt),
            0xcc => Ok(Commands::WriteDmaQueued),
            0x36 => Ok(Commands::WriteDmaQueuedExt),
            0xe7 => Ok(Commands::CacheFlush),
            0xea => Ok(Commands::CasheFlushExt),
            _ => Err(AtaError::InvalidValue),
        }
    }
}
