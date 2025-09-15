// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::collections::BTreeMap;
use std::num::NonZeroUsize;
use std::path::Path;
use std::str::FromStr;

use serde_derive::{Deserialize, Serialize};
use thiserror::Error;

pub use cpuid_profile_config::CpuidProfile;

pub mod spec;

/// Configuration for the Propolis server.
// NOTE: This is expected to change over time; portions of the hard-coded
// configuration will likely become more dynamic.
#[derive(Serialize, Deserialize, Debug, PartialEq)]
pub struct Config {
    #[serde(default, rename = "pci_bridge")]
    pub pci_bridges: Vec<PciBridge>,

    #[serde(default)]
    pub chipset: Chipset,

    #[serde(default, rename = "dev")]
    pub devices: BTreeMap<String, Device>,

    #[serde(default, rename = "block_dev")]
    pub block_devs: BTreeMap<String, BlockDevice>,

    #[serde(default, rename = "cpuid")]
    pub cpuid_profiles: BTreeMap<String, CpuidProfile>,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            pci_bridges: Vec::new(),
            chipset: Chipset { options: BTreeMap::new() },
            devices: BTreeMap::new(),
            block_devs: BTreeMap::new(),
            cpuid_profiles: BTreeMap::new(),
        }
    }
}

/// The instance's chipset.
#[derive(Default, Serialize, Deserialize, Debug, PartialEq)]
pub struct Chipset {
    #[serde(flatten, default)]
    pub options: BTreeMap<String, toml::Value>,
}

impl Chipset {
    pub fn get_string<S: AsRef<str>>(&self, key: S) -> Option<&str> {
        self.options.get(key.as_ref())?.as_str()
    }

    pub fn get<T: FromStr, S: AsRef<str>>(&self, key: S) -> Option<T> {
        self.get_string(key)?.parse().ok()
    }
}

/// A PCI-PCI bridge.
#[derive(Default, Serialize, Deserialize, Debug, PartialEq, Eq)]
pub struct PciBridge {
    /// The bus/device/function of this bridge as a device in the PCI topology.
    #[serde(rename = "pci-path")]
    pub pci_path: String,

    /// The logical bus number to assign to this bridge's downstream bus.
    ///
    /// Note: This bus number is only used at configuration time to attach
    /// devices downstream of this bridge. The bridge's secondary bus number
    /// (used by the guest to address traffic to devices on this bus) is
    /// set by the guest at runtime.
    #[serde(rename = "downstream-bus")]
    pub downstream_bus: u8,
}

/// A hard-coded device, either enabled by default or accessible locally
/// on a machine.
#[derive(Clone, Serialize, Deserialize, Debug, PartialEq)]
pub struct Device {
    pub driver: String,

    #[serde(flatten, default)]
    pub options: BTreeMap<String, toml::Value>,
}

impl Device {
    pub fn get_string<S: AsRef<str>>(&self, key: S) -> Option<&str> {
        self.options.get(key.as_ref())?.as_str()
    }

    pub fn get<T: FromStr, S: AsRef<str>>(&self, key: S) -> Option<T> {
        self.get_string(key)?.parse().ok()
    }
}

#[derive(Debug, Deserialize, Serialize, PartialEq)]
pub struct BlockOpts {
    pub block_size: Option<u32>,
    pub read_only: Option<bool>,
    pub skip_flush: Option<bool>,
    pub workers: Option<NonZeroUsize>,
}

#[derive(Serialize, Deserialize, Debug, PartialEq)]
pub struct BlockDevice {
    #[serde(default, rename = "type")]
    pub bdtype: String,

    #[serde(flatten)]
    pub opts: BlockOpts,

    #[serde(flatten, default)]
    pub options: BTreeMap<String, toml::Value>,
}

/// Errors which may be returned when parsing the server configuration.
#[derive(Error, Debug)]
pub enum ParseError {
    #[error("Cannot parse toml: {0}")]
    Toml(#[from] toml::de::Error),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Key {0} not found in {1}")]
    KeyNotFound(String, String),

    #[error("Could not unmarshall {0} with function {1}")]
    AsError(String, String),
}

/// Parses a TOML file into a configuration object.
pub fn parse<P: AsRef<Path>>(path: P) -> Result<Config, ParseError> {
    let contents = std::fs::read_to_string(path.as_ref())?;
    let cfg = toml::from_str::<Config>(&contents)?;
    Ok(cfg)
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn config_can_be_serialized_as_toml() {
        let dummy_config = Config { ..Default::default() };
        let serialized = toml::ser::to_string(&dummy_config).unwrap();
        let deserialized: Config = toml::de::from_str(&serialized).unwrap();
        assert_eq!(dummy_config, deserialized);
    }

    #[test]
    fn parse_basic_config() {
        let raw = r#"
[chipset]
chipset-opt = "copt"

[dev.drv0]
driver = "nvme"
other-opt = "value"

[dev.drv1]
driver = "widget"
foo = "bar"

[block_dev.block0]
type = "cement"
slump = "4in"

[block_dev.block1]
type = "file"
path = "/etc/passwd"
"#;
        let cfg: Config = toml::de::from_str(raw).unwrap();

        use toml::Value;

        assert_eq!(cfg.chipset.get_string("chipset-opt"), Some("copt"));

        assert!(cfg.devices.contains_key("drv0"));
        assert!(cfg.devices.contains_key("drv1"));
        let dev0 = cfg.devices.get("drv0").unwrap();
        let dev1 = cfg.devices.get("drv1").unwrap();

        assert_eq!(dev0.driver, "nvme");
        assert_eq!(dev0.get_string("other-opt"), Some("value"));
        assert_eq!(dev1.driver, "widget");
        assert_eq!(dev1.get_string("foo"), Some("bar"));

        assert!(cfg.block_devs.contains_key("block0"));
        assert!(cfg.block_devs.contains_key("block1"));
        let bdev0 = cfg.block_devs.get("block0").unwrap();
        let bdev1 = cfg.block_devs.get("block1").unwrap();

        assert_eq!(bdev0.bdtype, "cement");
        assert_eq!(
            bdev0.options.get("slump").map(Value::as_str).unwrap(),
            Some("4in")
        );
        assert_eq!(bdev1.bdtype, "file");
        assert_eq!(
            bdev1.options.get("path").map(Value::as_str).unwrap(),
            Some("/etc/passwd")
        );
    }
}
