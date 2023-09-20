// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

#![allow(dead_code)]

use crate::hw::ata::AtaError;
use bitstruct::*;
use std::fmt;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Registers {
    Data16,
    Data32,
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
    DeviceReset = 0x08,
    Recalibrate = 0x10,
    ReadSectors = 0x20,
    ReadSectorsWithoutRetry = 0x21,
    ReadSectorsExt = 0x24,
    ReadDmaExt = 0x25,
    ReadDmaQueuedExt = 0x26,
    WriteDmaExt = 0x35,
    WriteDmaQueuedExt = 0x36,
    ExecuteDeviceDiagnostics = 0x90,
    InitializeDeviceParameters = 0x91,
    DownloadMicrocode = 0x92,
    Packet = 0xa0,
    IdentifyPacketDevice = 0xa1,
    SetMultipleMode = 0xc6,
    ReadDmaQueued = 0xc7,
    ReadDma = 0xc8,
    ReadDmaWithoutRetry = 0xc9,
    WriteDma = 0xca,
    WriteDmaWithoutRetry = 0xcb,
    WriteDmaQueued = 0xcc,
    IdleImmediate = 0xe1,
    Idle = 0xe3,
    CacheFlush = 0xe7,
    CasheFlushExt = 0xea,
    IdenfityDevice = 0xec,
    SetFeatures = 0xef,
}

impl From<Commands> for u8 {
    fn from(c: Commands) -> Self {
        c as u8
    }
}

impl PartialEq<Commands> for u8 {
    fn eq(&self, c: &Commands) -> bool {
        *self == *c as u8
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
        use Commands::*;

        match code {
            0x00 => Ok(Nop),
            0x08 => Ok(DeviceReset),
            0x10 => Ok(Recalibrate),
            0x20 => Ok(ReadSectors),
            0x21 => Ok(ReadSectorsWithoutRetry),
            0x24 => Ok(ReadSectorsExt),
            0x25 => Ok(ReadDmaExt),
            0x26 => Ok(ReadDmaQueuedExt),
            0x35 => Ok(WriteDmaExt),
            0x36 => Ok(WriteDmaQueuedExt),
            0x90 => Ok(ExecuteDeviceDiagnostics),
            0x91 => Ok(InitializeDeviceParameters),
            0x92 => Ok(DownloadMicrocode),
            0xa0 => Ok(Packet),
            0xa1 => Ok(IdentifyPacketDevice),
            0xc6 => Ok(SetMultipleMode),
            0xc7 => Ok(ReadDmaQueued),
            0xc8 => Ok(ReadDma),
            0xc9 => Ok(ReadDmaWithoutRetry),
            0xca => Ok(WriteDma),
            0xcb => Ok(WriteDmaWithoutRetry),
            0xcc => Ok(WriteDmaQueued),
            0xe1 => Ok(IdleImmediate),
            0xe3 => Ok(Idle),
            0xe7 => Ok(CacheFlush),
            0xea => Ok(CasheFlushExt),
            0xec => Ok(IdenfityDevice),
            0xef => Ok(SetFeatures),
            _ => Err(AtaError::UnknownCommandCode(code)),
        }
    }
}

bitstruct! {
    /// Representation of the Status register.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct StatusRegister(pub(super) u8) {
        pub error: bool = 0;
        pub data_request: bool = 3;
        pub device_fault: bool = 5;
        pub device_ready: bool = 6;
        pub busy: bool = 7;

        // Command dependent bits.
        pub device_seek_complete: bool = 4;

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

impl FromRaw<u8, Self> for StatusRegister {
    fn from_raw(raw: u8) -> Self {
        Self(raw)
    }
}

bitstruct! {
    /// Representation of the Status register.
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    pub struct ErrorRegister(pub(super) u8) {
        pub abort: bool = 2;
    }
}

impl FromRaw<u8, Self> for ErrorRegister {
    fn from_raw(raw: u8) -> Self {
        Self(raw)
    }
}

bitstruct! {
    #[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
    pub struct DeviceRegister(pub(super) u8) {
        pub heads: u8 = 0..4;
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
