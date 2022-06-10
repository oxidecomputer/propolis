//! Describes a server config which may be parsed from a TOML file.

use std::collections::{btree_map, BTreeMap};
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;

use propolis::hw::pci::topology::BridgeDescription;
use propolis::hw::pci::Bdf;
use serde_derive::{Deserialize, Serialize};
use thiserror::Error;

use propolis::block;
use propolis::dispatch::Dispatcher;
use propolis::hw::pci;
use propolis::inventory;

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

/// Configuration for the Propolis server.
// NOTE: This is expected to change over time; portions of the hard-coded
// configuration will likely become more dynamic.
#[derive(Serialize, Deserialize, Debug, PartialEq)]
pub struct Config {
    bootrom: PathBuf,

    #[serde(default, rename = "pci_bridge")]
    pci_bridges: Vec<PciBridge>,

    #[serde(default)]
    chipset: Chipset,

    #[serde(default, rename = "dev")]
    devices: BTreeMap<String, Device>,

    #[serde(default, rename = "block_dev")]
    block_devs: BTreeMap<String, BlockDevice>,
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

    pub fn get_bootrom(&self) -> &Path {
        &self.bootrom
    }

    pub fn get_chipset(&self) -> &Chipset {
        &self.chipset
    }

    pub fn get_pci_bridge_descriptions(
        &self,
    ) -> Vec<pci::topology::BridgeDescription> {
        let mut descs = Vec::with_capacity(self.pci_bridges.len());
        for bridge in &self.pci_bridges {
            descs.push(BridgeDescription::new(
                pci::topology::LogicalBusId(bridge.downstream_bus),
                Bdf::from_str(bridge.pci_path.as_str()).unwrap(),
            ));
        }
        descs
    }

    pub fn devs(&self) -> IterDevs {
        IterDevs { inner: self.devices.iter() }
    }

    pub fn create_block_backend(
        &self,
        name: &str,
        disp: &Dispatcher,
    ) -> Result<(Arc<dyn block::Backend>, inventory::ChildRegister), ParseError>
    {
        let entry = self.block_devs.get(name).ok_or_else(|| {
            ParseError::KeyNotFound(name.to_string(), "block_dev".to_string())
        })?;
        entry.create_block_backend(disp)
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
#[derive(Default, Serialize, Deserialize, Debug, PartialEq)]
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

impl BlockDevice {
    pub fn create_block_backend(
        &self,
        _disp: &Dispatcher,
    ) -> Result<(Arc<dyn block::Backend>, inventory::ChildRegister), ParseError>
    {
        match &self.bdtype as &str {
            "file" => {
                let path = self
                    .options
                    .get("path")
                    .ok_or_else(|| {
                        ParseError::KeyNotFound(
                            "path".to_string(),
                            "options".to_string(),
                        )
                    })?
                    .as_str()
                    .ok_or_else(|| {
                        ParseError::AsError(
                            "path".to_string(),
                            "as_str".to_string(),
                        )
                    })?;

                let readonly: bool = || -> Option<bool> {
                    self.options.get("readonly")?.as_str()?.parse().ok()
                }()
                .unwrap_or(false);
                let nworkers = NonZeroUsize::new(8).unwrap();
                let be = propolis::block::FileBackend::create(
                    path, readonly, nworkers,
                )?;
                let child =
                    inventory::ChildRegister::new(&be, Some(path.to_string()));

                Ok((be, child))
            }
            _ => {
                panic!("unrecognized block dev type {}!", self.bdtype);
            }
        }
    }
}

/// Iterator returned from [`Config::devs`] which allows iteration over
/// all [`Device`] objects.
pub struct IterDevs<'a> {
    inner: btree_map::Iter<'a, String, Device>,
}

impl<'a> Iterator for IterDevs<'a> {
    type Item = (&'a String, &'a Device);

    fn next(&mut self) -> Option<Self::Item> {
        self.inner.next()
    }
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
