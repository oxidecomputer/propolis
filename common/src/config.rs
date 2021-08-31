use std::collections::{btree_map, BTreeMap};
use std::str::FromStr;
use std::sync::Arc;

use serde_derive::Deserialize;

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

#[derive(Deserialize, Debug)]
pub struct BlockDevice {
    #[serde(default, rename = "type")]
    pub bdtype: String,

    #[serde(flatten, default)]
    pub options: BTreeMap<String, toml::Value>,
}

impl BlockDevice {
    pub fn block_dev<R: propolis::block::BlockReq>(
        &self,
    ) -> Arc<dyn propolis::block::BlockDev<R>> {
        match &self.bdtype as &str {
            "file" => {
                let path = self.options.get("path").unwrap().as_str().unwrap();

                let readonly: bool = || -> Option<bool> {
                    self.options.get("readonly")?.as_str()?.parse().ok()
                }()
                .unwrap_or(false);

                propolis::block::FileBdev::<R>::create(path, readonly).unwrap()
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
    pub inner: btree_map::Iter<'a, String, Device>,
}

impl<'a> Iterator for IterDevs<'a> {
    type Item = (&'a String, &'a Device);

    fn next(&mut self) -> Option<Self::Item> {
        self.inner.next()
    }
}
