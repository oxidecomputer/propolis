use std::num::NonZeroUsize;
use std::str::FromStr;
use std::sync::Arc;

use anyhow::Context;

use propolis::block;
use propolis::hw::pci::Bdf;
use propolis::inventory::ChildRegister;

use propolis_standalone_config::Device;
pub use propolis_standalone_config::{Config, SnapshotTag};

pub fn block_backend(
    config: &Config,
    dev: &Device,
    log: &slog::Logger,
) -> (Arc<dyn block::Backend>, ChildRegister) {
    let backend_name = dev.options.get("block_dev").unwrap().as_str().unwrap();
    let be = config.block_devs.get(backend_name).unwrap();

    match &be.bdtype as &str {
        "file" => {
            let path = be.options.get("path").unwrap().as_str().unwrap();

            let readonly: bool = || -> Option<bool> {
                match be.options.get("readonly") {
                    Some(toml::Value::Boolean(read_only)) => Some(*read_only),
                    Some(toml::Value::String(v)) => v.parse().ok(),
                    _ => None,
                }
            }()
            .unwrap_or(false);

            let be = block::FileBackend::create(
                path,
                readonly,
                NonZeroUsize::new(8).unwrap(),
                log.clone(),
            )
            .unwrap();

            let creg = ChildRegister::new(&be, Some(path.to_string()));
            (be, creg)
        }
        _ => {
            panic!("unrecognized block dev type {}!", be.bdtype);
        }
    }
}

pub fn parse(path: &str) -> anyhow::Result<Config> {
    let file_data =
        std::fs::read(path).context("Failed to read given config.toml")?;
    Ok(toml::from_slice::<Config>(&file_data)?)
}

pub fn parse_bdf(v: &str) -> Option<Bdf> {
    let mut fields = Vec::with_capacity(3);
    for f in v.split('.') {
        let num = usize::from_str(f).ok()?;
        if num > u8::MAX as usize {
            return None;
        }
        fields.push(num as u8);
    }

    if fields.len() == 3 {
        Bdf::new(fields[0], fields[1], fields[2])
    } else {
        None
    }
}
