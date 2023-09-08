// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

#![allow(dead_code)]

use thiserror::Error;

mod bits;
mod controller;
mod device;

pub use bits::{Commands, Registers};
pub use controller::AtaController;
pub use device::AtaDevice;

#[derive(Debug, Error)]
pub enum AtaError {
    #[error("no device")]
    NoDevice,

    #[error("device not ready")]
    DeviceNotReady,

    #[error("unknown command code ({0})")]
    UnknownCommandCode(u8),

    #[error("unsupported command ({0})")]
    UnsupportedCommand(Commands),
}

#[usdt::provider(provider = "propolis")]
mod probes {
    fn ata_cmd(cmd: u8) {}
}
