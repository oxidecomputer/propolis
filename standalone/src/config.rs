use std::collections::{btree_map, BTreeMap};
use std::num::NonZeroUsize;
use std::str::FromStr;
use std::sync::Arc;

use anyhow::Context;
use serde::{Deserialize, Serialize};

use crate::hw::pci;
use propolis::block;
use propolis::dispatch::Dispatcher;
use propolis::inventory::ChildRegister;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Config {
    main: Main,

    #[serde(default, rename = "dev")]
    devices: BTreeMap<String, Device>,

    #[serde(default, rename = "block_dev")]
    block_devs: BTreeMap<String, BlockDevice>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct Main {
    name: String,
    cpus: u8,
    bootrom: String,
    memory: usize,
}

impl Config {
    pub fn get_name(&self) -> &String {
        &self.main.name
    }
    pub fn get_cpus(&self) -> u8 {
        self.main.cpus
    }
    pub fn get_mem(&self) -> usize {
        self.main.memory
    }
    pub fn get_bootrom(&self) -> &String {
        &self.main.bootrom
    }
    pub fn devs(&self) -> IterDevs {
        IterDevs { inner: self.devices.iter() }
    }

    pub fn block_dev(
        &self,
        name: &str,
        disp: &Dispatcher,
    ) -> (Arc<dyn block::Backend>, ChildRegister) {
        let entry = self.block_devs.get(name).unwrap();
        entry.block_dev(disp)
    }
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
pub struct BlockDevice {
    #[serde(default, rename = "type")]
    pub bdtype: String,

    #[serde(flatten, default)]
    pub options: BTreeMap<String, toml::Value>,
}

impl BlockDevice {
    pub fn block_dev(
        &self,
        _disp: &Dispatcher,
    ) -> (Arc<dyn block::Backend>, ChildRegister) {
        match &self.bdtype as &str {
            "file" => {
                let path = self.options.get("path").unwrap().as_str().unwrap();

                let readonly: bool = || -> Option<bool> {
                    match self.options.get("readonly") {
                        Some(toml::Value::Boolean(read_only)) => {
                            Some(*read_only)
                        }
                        Some(toml::Value::String(v)) => v.parse().ok(),
                        _ => None,
                    }
                }()
                .unwrap_or(false);

                let be = block::FileBackend::create(
                    path,
                    readonly,
                    NonZeroUsize::new(8).unwrap(),
                )
                .unwrap();

                let creg = ChildRegister::new(&be, Some(path.to_string()));
                (be, creg)
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

pub fn parse(path: &str) -> anyhow::Result<Config> {
    let file_data =
        std::fs::read(path).context("Failed to read given config.toml")?;
    Ok(toml::from_slice::<Config>(&file_data)?)
}

pub fn parse_bdf(v: &str) -> Option<pci::Bdf> {
    let mut fields = Vec::with_capacity(3);
    for f in v.split('.') {
        let num = usize::from_str(f).ok()?;
        if num > u8::MAX as usize {
            return None;
        }
        fields.push(num as u8);
    }

    if fields.len() == 3 {
        pci::Bdf::new(fields[0], fields[1], fields[2])
    } else {
        None
    }
}
