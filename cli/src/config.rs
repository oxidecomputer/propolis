use std::collections::BTreeMap;
use std::str::FromStr;
use std::sync::Arc;

use serde_derive::Deserialize;

use crate::hw::pci;
use propolis::block::{BlockDev, BlockReq};

use common::config::{BlockDevice, Device, IterDevs};

#[derive(Deserialize, Debug)]
struct Top {
    main: Main,

    #[serde(default, rename = "dev")]
    devices: BTreeMap<String, Device>,

    #[serde(default, rename = "block_dev")]
    block_devs: BTreeMap<String, BlockDevice>,
}

#[derive(Deserialize, Debug)]
struct Main {
    name: String,
    cpus: u8,
    bootrom: String,
    memory: usize,
}

pub struct Config {
    inner: Top,
}
impl Config {
    pub fn get_name(&self) -> &String {
        &self.inner.main.name
    }
    pub fn get_cpus(&self) -> u8 {
        self.inner.main.cpus
    }
    pub fn get_mem(&self) -> usize {
        self.inner.main.memory
    }
    pub fn get_bootrom(&self) -> &String {
        &self.inner.main.bootrom
    }
    pub fn devs(&self) -> IterDevs {
        IterDevs { inner: self.inner.devices.iter() }
    }

    pub fn block_dev<R: BlockReq>(&self, name: &str) -> Arc<dyn BlockDev<R>> {
        let entry = self.inner.block_devs.get(name).unwrap();
        entry.block_dev::<R>()
    }
}

pub fn parse(path: &str) -> Config {
    let file_data = std::fs::read(path).unwrap();
    let top = toml::from_slice::<Top>(&file_data).unwrap();
    Config { inner: top }
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
        pci::Bdf::try_new(fields[0], fields[1], fields[2])
    } else {
        None
    }
}
