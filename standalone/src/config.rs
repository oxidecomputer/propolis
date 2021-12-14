use std::collections::{btree_map, BTreeMap};
use std::net::SocketAddr;
use std::num::NonZeroUsize;
use std::str::FromStr;
use std::sync::Arc;

use serde_derive::Deserialize;

use crate::hw::pci;
use propolis::block;
use propolis::dispatch::Dispatcher;
use propolis::inventory::ChildRegister;

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

    pub fn block_dev(
        &self,
        name: &str,
        disp: &Dispatcher,
    ) -> (Arc<dyn block::Backend>, ChildRegister) {
        let entry = self.inner.block_devs.get(name).unwrap();
        entry.block_dev(disp)
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

#[derive(Deserialize, Debug)]
pub struct BlockDevice {
    #[serde(default, rename = "type")]
    pub bdtype: String,

    #[serde(flatten, default)]
    pub options: BTreeMap<String, toml::Value>,
}

impl BlockDevice {
    pub fn block_dev(
        &self,
        disp: &Dispatcher,
    ) -> (Arc<dyn block::Backend>, ChildRegister) {
        match &self.bdtype as &str {
            "file" => {
                let path = self.options.get("path").unwrap().as_str().unwrap();

                let readonly: bool = || -> Option<bool> {
                    self.options.get("readonly")?.as_str()?.parse().ok()
                }()
                .unwrap_or(false);

                let be = block::FileBackend::create(
                    path,
                    readonly,
                    NonZeroUsize::new(8).unwrap(),
                )
                .unwrap();

                let creg = ChildRegister::new(&be, None);
                (be, creg)
            }
            "crucible" => {
                let mut targets: Vec<SocketAddr> = Vec::new();

                for target in self
                    .options
                    .get("targets")
                    .unwrap()
                    .as_array()
                    .unwrap()
                    .to_vec()
                {
                    let addr =
                        target.as_str().unwrap().to_string().parse().unwrap();
                    targets.push(addr);
                }

                let read_only: bool = || -> Option<bool> {
                    self.options.get("readonly")?.as_str()?.parse().ok()
                }()
                .unwrap_or(false);

                let key: Option<String> = self
                    .options
                    .get("key")
                    .map(|x| x.as_str().unwrap().to_string());
                let gen: Option<u64> = self
                    .options
                    .get("gen")
                    .map(|x| x.as_str())
                    .flatten()
                    .map(|x| u64::from_str(x).ok())
                    .flatten();

                let be = propolis::block::CrucibleBackend::create(
                    disp, targets, read_only, key, gen,
                )
                .unwrap();

                // TODO: use volume ID or something for instance name
                let creg = ChildRegister::new(&be, None);
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
        pci::Bdf::new(fields[0], fields[1], fields[2])
    } else {
        None
    }
}
