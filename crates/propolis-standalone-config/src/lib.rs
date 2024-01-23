// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use strum::FromRepr;

pub use cpuid_profile_config::*;

#[derive(FromRepr, Eq, PartialEq)]
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

    #[serde(default, rename = "cpuid")]
    pub cpuid_profiles: BTreeMap<String, CpuidProfile>,

    pub cloudinit: Option<CloudInit>,
}
impl Config {
    pub fn cpuid_profile(&self) -> Option<&CpuidProfile> {
        match self.main.cpuid_profile.as_ref() {
            Some(name) => self.cpuid_profiles.get(name),
            None => None,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Main {
    pub name: String,
    pub cpus: u8,
    pub bootrom: String,
    pub memory: usize,
    pub use_reservoir: Option<bool>,
    pub qemu_debug_file: Option<PathBuf>,
    pub cpuid_profile: Option<String>,
    /// Process exitcode to emit if/when instance halts
    ///
    /// Default: 0
    #[serde(default)]
    pub exit_on_halt: u8,
    /// Process exitcode to emit if/when instance reboots
    ///
    /// Default: None, does not exit on reboot
    #[serde(default)]
    pub exit_on_reboot: Option<u8>,
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

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub struct CloudInit {
    pub user_data: Option<String>,
    pub meta_data: Option<String>,
    pub network_config: Option<String>,

    // allow path-style contents as well
    pub user_data_path: Option<String>,
    pub meta_data_path: Option<String>,
    pub network_config_path: Option<String>,
}
