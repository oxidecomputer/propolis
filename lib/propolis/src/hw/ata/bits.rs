// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

#![allow(dead_code)]

use std::fmt;

use crate::hw::ata::AtaError;

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

// use bitstruct::bitstruct;

// mod Registers {
//     bitstruct! {
//         /// Representation of the Status register.
//         #[derive(Clone, Copy, Debug, Default, From)]
//         pub struct Status(pub(crate) u8) {
//             pub error: bool = 0;
//             obsolete1: bool = 1;
//             obsolete2: bool = 2;
//             pub data_request: bool = 3;
//             mode_specific_1: bool = 4;
//             pub device_fault: bool = 5;
//             pub device_ready: bool = 6;
//             pub busy: bool = 7;
//         }
//     }

//     bitstruct! {
//         #[derive(Copy, Clone, PartialEq, Eq, From)]
//         pub struct Device(pub(crate) u8) {
//             pub address: u8 = 0..3;
//             pub device_select: bool = 4;
//             obsolete1: u8 = 5;
//             pub lba_addressing: bool = 6;
//             obsolete2: u8 = 7;
//         }
//     }

//     impl Default for Device {
//         fn default() -> Self {
//             // Make sure default bits are set.
//             Self(0xa0)
//         }
//     }
// }
