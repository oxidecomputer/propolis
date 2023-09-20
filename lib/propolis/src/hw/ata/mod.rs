// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

#![allow(dead_code)]

use slog::{Record, Result, Serializer, KV};
use thiserror::Error;

mod bits;
pub mod controller;
pub mod device;
pub mod geometry;
pub mod pci;
#[cfg(test)]
mod test;

pub use bits::{Commands, Registers};
pub use controller::AtaController;
pub use device::AtaDevice;

const BLOCK_SIZE: usize = 512;

#[derive(Debug, Error)]
pub enum AtaError {
    #[error("no device")]
    NoDevice,

    #[error("device is busy")]
    DeviceBusy,

    #[error("device not ready")]
    DeviceNotReady,

    #[error("no master boot record")]
    NoMasterBootRecord,

    #[error("unknown command code ({0})")]
    UnknownCommandCode(u8),

    #[error("unsupported command ({0})")]
    UnsupportedCommand(Commands),

    #[error("feature not supported ({0})")]
    FeatureNotSupported(u8),

    #[error("transfer mode not supported ({0}, {0})")]
    TransferModeNotSupported(u8, u8),
}

impl KV for AtaError {
    fn serialize(
        &self,
        _rec: &Record,
        serializer: &mut dyn Serializer,
    ) -> Result {
        serializer.emit_str("error", &self.to_string())
    }
}

#[usdt::provider(provider = "propolis")]
mod probes {
    fn ata_cmd(cmd: u8) {}
}
