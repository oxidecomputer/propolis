// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

#![allow(dead_code)]

use crate::hw::ata::AtaError;
use bitstruct::bitstruct;
use std::fmt;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Registers {
    Data,
    Error,
    Features,
    SectorCount,
    LbaLow,
    LbaMid,
    LbaHigh,
    Device,
    Status,
    Command,
    AltStatus,
    DeviceControl,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Commands {
    Nop = 0x00,
    ExecuteDeviceDiagnostics = 0x90,
    IdenfityDevice = 0xec,
    IdentifyPacketDevice = 0xa1,
    SetFeatures = 0xef,
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

impl PartialEq<Commands> for u16 {
    fn eq(&self, c: &Commands) -> bool {
        *self as u8 == *c as u8
    }
}

impl fmt::Display for Commands {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{:?}", self)
    }
}

impl TryFrom<u8> for Commands {
    type Error = AtaError;

    fn try_from(code: u8) -> Result<Self, Self::Error> {
        match code {
            0x00 => Ok(Commands::Nop),
            0x90 => Ok(Commands::ExecuteDeviceDiagnostics),
            0xec => Ok(Commands::IdenfityDevice),
            0xa1 => Ok(Commands::IdentifyPacketDevice),
            0xef => Ok(Commands::SetFeatures),
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
            _ => Err(AtaError::UnknownCommandCode(code)),
        }
    }
}

bitstruct! {
    /// Representation of the Status register.
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    pub struct StatusRegister(pub(super) u8) {
        pub error: bool = 0;
        pub data_request: bool = 3;
        pub device_fault: bool = 5;
        pub device_ready: bool = 6;
        pub busy: bool = 7;

        /// ATA8-ACS fields. Note that some of these fields are backed by
        /// repurposed bits after they went obsolete in prior versions of the
        /// ATA specification. As such they may only be valid when used in
        /// combination with a driver which supports these more recent modes.
        pub sense_data_available: bool = 1;
        pub alignment_error: bool = 2;
        pub deferred_write_error: bool = 4;
        pub stream_error: bool = 5;
    }
}

bitstruct! {
    /// Representation of the Status register.
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    pub struct ErrorRegister(pub(super) u8) {
        pub abort: bool = 2;
    }
}

bitstruct! {
    #[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
    pub struct DeviceRegister(pub(super) u8) {
        pub address: u8 = 0..3;
        pub device_select: bool = 4;
        pub lba_addressing: bool = 6;
    }
}

bitstruct! {
    #[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
    pub struct DeviceControlRegister(pub(super) u8) {
        pub interrupt_enabled_n: bool = 1;
        pub software_reset: bool = 2;
        pub high_order_byte: bool = 7;
    }
}
