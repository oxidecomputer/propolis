//! Describes a server config which may be parsed from a TOML file.

use std::collections::{btree_map, BTreeMap};
use std::net::SocketAddr;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;

use serde_derive::{Deserialize, Serialize};
use thiserror::Error;

use propolis::block;
use propolis::dispatch::Dispatcher;
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
#[derive(Serialize, Deserialize, Debug)]
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

/// A hard-coded device, either enabled by default or accessible locally
/// on a machine.
#[derive(Serialize, Deserialize, Debug)]
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

#[derive(Serialize, Deserialize, Debug)]
pub struct BlockDevice {
    #[serde(default, rename = "type")]
    pub bdtype: String,

    #[serde(flatten, default)]
    pub options: BTreeMap<String, toml::Value>,
}

impl BlockDevice {
    pub fn create_block_backend(
        &self,
        disp: &Dispatcher,
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
                let child = inventory::ChildRegister::new(&be, None);

                Ok((be, child))
            }
            "crucible" => {
                let targets: Vec<SocketAddr> = self
                    .options
                    .get("targets")
                    .ok_or_else(|| {
                        ParseError::KeyNotFound(
                            "targets".to_string(),
                            "options".to_string(),
                        )
                    })?
                    .as_array()
                    .ok_or_else(|| {
                        ParseError::AsError(
                            "targets".to_string(),
                            "as_array".to_string(),
                        )
                    })?
                    .to_vec()
                    .iter()
                    .map(|x| {
                        x.as_str()
                            .ok_or_else(|| {
                                ParseError::AsError(
                                    "x".to_string(),
                                    "as_str".to_string(),
                                )
                            })?
                            .parse()
                            .map_err(|e| {
                                ParseError::AsError(
                                    "x".to_string(),
                                    format!("parse(): {:?}", e),
                                )
                            })
                    })
                    .collect::<Result<Vec<SocketAddr>, ParseError>>()?;

                let read_only: bool = || -> Option<bool> {
                    self.options.get("readonly")?.as_str()?.parse().ok()
                }()
                .unwrap_or(false);

                let key: Option<String> = self
                    .options
                    .get("key")
                    .map(|x| -> Result<String, ParseError> {
                        Ok(x.as_str()
                            .ok_or_else(|| {
                                ParseError::AsError(
                                    "x".to_string(),
                                    "as_str".to_string(),
                                )
                            })?
                            .to_string())
                    })
                    .map_or(Ok(None), |r| r.map(Some))?;
                let gen: Option<u64> = self
                    .options
                    .get("gen")
                    .map(|x| x.as_str())
                    .flatten()
                    .map(|x| u64::from_str(x).ok())
                    .flatten();

                let be = propolis::block::CrucibleBackend::create(
                    disp, targets, read_only, key, gen,
                )?;

                // TODO: use volume ID or something for instance name
                let creg = inventory::ChildRegister::new(&be, None);

                Ok((be, creg))
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
