//! Describes a server config which may be parsed from a TOML file.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_derive::Deserialize;
use thiserror::Error;

use common::config::{BlockDevice, Device, IterDevs};

/// Errors which may be returned when parsing the server configuration.
#[derive(Error, Debug)]
pub enum ParseError {
    #[error("Cannot parse toml: {0}")]
    Toml(#[from] toml::de::Error),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

/// Configuration for the Propolis server.
// NOTE: This is expected to change over time; portions of the hard-coded
// configuration will likely become more dynamic.
#[derive(Deserialize, Debug)]
pub struct Config {
    bootrom: PathBuf,

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
        devices: BTreeMap<String, Device>,
        block_devs: BTreeMap<String, BlockDevice>,
    ) -> Config {
        Config { bootrom: bootrom.into(), devices, block_devs }
    }

    pub fn get_bootrom(&self) -> &Path {
        &self.bootrom
    }

    pub fn devs(&self) -> IterDevs {
        IterDevs { inner: self.devices.iter() }
    }

    pub fn block_dev<R: propolis::block::BlockReq>(
        &self,
        name: &str,
    ) -> Arc<dyn propolis::block::BlockDev<R>> {
        let entry = self.block_devs.get(name).unwrap();
        entry.block_dev::<R>()
    }
}

/// Parses a TOML file into a configuration object.
pub fn parse<P: AsRef<Path>>(path: P) -> Result<Config, ParseError> {
    let contents = std::fs::read_to_string(path.as_ref())?;
    let cfg = toml::from_str::<Config>(&contents)?;
    Ok(cfg)
}
