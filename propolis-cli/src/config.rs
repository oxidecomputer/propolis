use std::collections::{btree_map, BTreeMap};
use std::str::FromStr;

use serde_derive::Deserialize;

use crate::hw::pci;

#[derive(Deserialize, Debug)]
struct Top {
    main: Main,

    #[serde(default, rename = "dev")]
    devices: BTreeMap<String, Device>,
}

#[derive(Deserialize, Debug)]
struct Main {
    name: String,
    cpus: u8,
    bootrom: String,
    memory: usize,
}

#[derive(Deserialize, Debug)]
pub struct Device {
    pub driver: String,

    #[serde(flatten, default)]
    pub options: BTreeMap<String, toml::Value>,
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
}
pub struct IterDevs<'a> {
    inner: btree_map::Iter<'a, String, Device>,
}
impl<'a> Iterator for IterDevs<'a> {
    type Item = (&'a String, &'a Device);

    fn next(&mut self) -> Option<Self::Item> {
        self.inner.next()
    }
}

pub fn parse(path: &str) -> Config {
    let file_data = std::fs::read(path).unwrap();
    let top = toml::from_slice::<Top>(&file_data).unwrap();
    Config { inner: top }
}

pub fn parse_bdf(v: &str) -> Option<pci::BDF> {
    let mut fields = Vec::with_capacity(3);
    for f in v.split('.') {
        let num = usize::from_str(f).ok()?;
        if num > u8::MAX as usize {
            return None;
        }
        fields.push(num as u8);
    }

    if fields.len() == 3 {
        pci::BDF::try_new(fields[0], fields[1], fields[2])
    } else {
        None
    }
}
