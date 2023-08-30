// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::collections::BTreeMap;

use num_enum::{IntoPrimitive, TryFromPrimitive};
use serde::{Deserialize, Serialize};

#[derive(TryFromPrimitive, IntoPrimitive, Eq, PartialEq)]
#[repr(u8)]
pub enum SnapshotTag {
    Config = 0,
    Global = 1,
    Device = 2,
    Lowmem = 3,
    Himem = 4,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Config {
    pub main: Main,

    #[serde(default, rename = "dev")]
    pub devices: BTreeMap<String, Device>,

    #[serde(default, rename = "block_dev")]
    pub block_devs: BTreeMap<String, BlockDevice>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Main {
    pub name: String,
    pub cpus: u8,
    pub bootrom: String,
    pub memory: usize,
    pub use_reservoir: Option<bool>,
}

/// A hard-coded device, either enabled by default or accessible locally
/// on a machine.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Device {
    pub driver: String,

    #[serde(flatten, default)]
    pub options: BTreeMap<String, toml::Value>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct BlockOpts {
    pub block_size: Option<u32>,
    pub read_only: Option<bool>,
    pub skip_flush: Option<bool>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct BlockDevice {
    #[serde(default, rename = "type")]
    pub bdtype: String,

    #[serde(flatten)]
    pub block_opts: BlockOpts,

    #[serde(flatten, default)]
    pub options: BTreeMap<String, toml::Value>,
}
