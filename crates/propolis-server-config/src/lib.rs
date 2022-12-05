use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use serde_derive::{Deserialize, Serialize};
use thiserror::Error;

/// Configuration for the Propolis server.
// NOTE: This is expected to change over time; portions of the hard-coded
// configuration will likely become more dynamic.
#[derive(Serialize, Deserialize, Debug, PartialEq)]
pub struct Config {
    pub bootrom: PathBuf,

    #[serde(default, rename = "pci_bridge")]
    pub pci_bridges: Vec<PciBridge>,

    #[serde(default)]
    pub chipset: Chipset,

    #[serde(default, rename = "dev")]
    pub devices: BTreeMap<String, Device>,

    #[serde(default, rename = "block_dev")]
    pub block_devs: BTreeMap<String, BlockDevice>,
}
impl Config {
    /// Constructs a new configuration object.
    ///
    /// Typically, the configuration is parsed from a config
    /// file via [`parse`], but this method allows an alternative
    /// mechanism for initialization.
    pub fn new<P: Into<PathBuf>>(
        bootrom: P,
        chipset: Chipset,
        devices: BTreeMap<String, Device>,
        block_devs: BTreeMap<String, BlockDevice>,
        pci_bridges: Vec<PciBridge>,
    ) -> Config {
        Config {
            bootrom: bootrom.into(),
            pci_bridges,
            chipset,
            devices,
            block_devs,
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
#[derive(Serialize, Deserialize, Debug, PartialEq)]
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

#[derive(Serialize, Deserialize, Debug, PartialEq)]
pub struct BlockDevice {
    #[serde(default, rename = "type")]
    pub bdtype: String,

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
mod tests {
    use super::*;

    #[test]
    fn config_can_be_serialized_as_toml() {
        let dummy_config = Config::new(
            "/boot",
            Chipset { options: BTreeMap::new() },
            BTreeMap::new(),
            BTreeMap::new(),
            Vec::new(),
        );
        let serialized = toml::ser::to_string(&dummy_config).unwrap();
        let deserialized: Config = toml::de::from_str(&serialized).unwrap();
        assert_eq!(dummy_config, deserialized);
    }
}
