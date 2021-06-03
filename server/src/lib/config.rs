//! Describes a server config which may be parsed from a TOML file.

use std::collections::{btree_map, BTreeMap};
use std::path::{Path, PathBuf};
use std::str::FromStr;

use serde_derive::Deserialize;
use thiserror::Error;

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
    ) -> Config {
        Config { bootrom: bootrom.into(), devices }
    }

    pub fn get_bootrom(&self) -> &Path {
        &self.bootrom
    }

    pub fn devs(&self) -> IterDevs {
        IterDevs { inner: self.devices.iter() }
    }
}

/// A hard-coded device, either enabled by default or accessible locally
/// on a machine.
#[derive(Deserialize, Debug)]
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
