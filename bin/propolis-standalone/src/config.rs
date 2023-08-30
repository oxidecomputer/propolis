// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::collections::BTreeMap;
use std::num::NonZeroUsize;
use std::str::FromStr;
use std::sync::Arc;

use anyhow::Context;
use serde::Deserialize;

use propolis::block;
use propolis::hw::pci::Bdf;
use propolis::inventory::ChildRegister;

use propolis_standalone_config::Device;
pub use propolis_standalone_config::{Config, SnapshotTag};

#[derive(Deserialize)]
struct FileConfig {
    path: String,
    workers: Option<usize>,
}
#[derive(Deserialize)]
struct MemAsyncConfig {
    size: u64,
    workers: Option<usize>,
}

// Try to turn unmatched flattened options into a config struct
fn opt_deser<'de, T: Deserialize<'de>>(
    value: &BTreeMap<String, toml::Value>,
) -> Result<T, anyhow::Error> {
    let map = toml::map::Map::from_iter(value.clone());
    let config = map.try_into::<T>()?;
    Ok(config)
}

const DEFAULT_WORKER_COUNT: usize = 8;

pub fn block_backend(
    config: &Config,
    dev: &Device,
    log: &slog::Logger,
) -> (Arc<dyn block::Backend>, ChildRegister) {
    let backend_name = dev.options.get("block_dev").unwrap().as_str().unwrap();
    let be = config.block_devs.get(backend_name).unwrap();
    let opts = block::BackendOpts {
        block_size: be.block_opts.block_size,
        read_only: be.block_opts.read_only,
        skip_flush: be.block_opts.skip_flush,
    };

    match &be.bdtype as &str {
        "file" => {
            let parsed: FileConfig = opt_deser(&be.options).unwrap();

            let be = block::FileBackend::create(
                &parsed.path,
                opts,
                NonZeroUsize::new(
                    parsed.workers.unwrap_or(DEFAULT_WORKER_COUNT),
                )
                .unwrap(),
                log.clone(),
            )
            .unwrap();

            let creg = ChildRegister::new(&be, Some(parsed.path));
            (be, creg)
        }
        "crucible" => create_crucible_backend(be, opts, log),
        "mem-async" => {
            let parsed: MemAsyncConfig = opt_deser(&be.options).unwrap();

            let be = block::MemAsyncBackend::create(
                parsed.size,
                opts,
                NonZeroUsize::new(
                    parsed.workers.unwrap_or(DEFAULT_WORKER_COUNT),
                )
                .unwrap(),
            )
            .unwrap();

            let creg = ChildRegister::new(&be, None);
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
    Ok(toml::from_str::<Config>(
        std::str::from_utf8(&file_data)
            .context("config should be valid utf-8")?,
    )?)
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

#[cfg(feature = "crucible")]
fn create_crucible_backend(
    be: &propolis_standalone_config::BlockDevice,
    opts: block::BackendOpts,
    log: &slog::Logger,
) -> (Arc<dyn block::Backend>, ChildRegister) {
    use slog::info;
    use uuid::Uuid;

    info!(
        log,
        "Building a crucible VolumeConstructionRequest from options {:?}",
        be.options
    );

    // No defaults on here because we really shouldn't try and guess
    // what block size the downstairs is using. A lot of things
    // default to 512, but it's best not to assume it'll always be
    // that way.
    let block_size = opts.block_size.unwrap() as u64;
    let read_only = opts.read_only.unwrap_or(false);

    let blocks_per_extent =
        be.options.get("blocks_per_extent").unwrap().as_integer().unwrap()
            as u64;

    let extent_count =
        be.options.get("extent_count").unwrap().as_integer().unwrap() as u32;

    // Parse a UUID, or generate a random one if none is specified.
    // Reasonable in something primarily used for testing like
    // propolis-standalone, but you wouldn't want to do this in
    // prod.
    let uuid = be
        .options
        .get("upstairs_id")
        .map(|x| Uuid::parse_str(x.as_str().unwrap()).unwrap())
        .unwrap_or_else(Uuid::new_v4);

    // The actual addresses of the three downstairs we're going to connect to.
    let targets: Vec<_> = be
        .options
        .get("targets")
        .unwrap()
        .as_array()
        .unwrap()
        .iter()
        .map(|target_val| target_val.as_str().unwrap().parse().unwrap())
        .collect();
    // There is currently no universe where an amount of Downstairs
    // other than 3 is valid.
    assert_eq!(targets.len(), 3);

    let lossy =
        be.options.get("lossy").map(|x| x.as_bool().unwrap()).unwrap_or(false);

    let flush_timeout =
        be.options.get("flush_timeout").map(|x| x.as_integer().unwrap() as f32);

    let key = be
        .options
        .get("encryption_key")
        .map(|x| x.as_str().unwrap().to_string());

    let cert_pem =
        be.options.get("cert_pem").map(|x| x.as_str().unwrap().to_string());

    let key_pem =
        be.options.get("key_pem").map(|x| x.as_str().unwrap().to_string());

    let root_cert_pem = be
        .options
        .get("root_cert_pem")
        .map(|x| x.as_str().unwrap().to_string());

    let control_addr = be
        .options
        .get("control_addr")
        .map(|target_val| target_val.as_str().unwrap().parse().unwrap());

    // This needs to increase monotonically with each successive
    // connection to the downstairs. As a hack, you can set it to
    // the current system time, and this will usually give us a newer
    // generation than the last connection. NEVER do this in prod
    // EVER.
    let generation =
        be.options.get("generation").unwrap().as_integer().unwrap() as u64;

    let req = crucible_client_types::VolumeConstructionRequest::Region {
        block_size,
        blocks_per_extent,
        extent_count,
        opts: crucible_client_types::CrucibleOpts {
            id: uuid,
            target: targets,
            lossy,
            flush_timeout,
            key,
            cert_pem,
            key_pem,
            root_cert_pem,
            control: control_addr,
            read_only,
        },
        gen: generation,
    };
    info!(log, "Creating Crucible disk from request {:?}", req);
    // QUESTION: is producer_registry: None correct here?
    let be = block::CrucibleBackend::create(req, opts, None, None, log.clone())
        .unwrap();
    let creg =
        ChildRegister::new(&be, Some(be.get_uuid().unwrap().to_string()));
    (be, creg)
}

#[cfg(not(feature = "crucible"))]
fn create_crucible_backend(
    _be: &propolis_standalone_config::BlockDevice,
    _opts: block::BackendOpts,
    _log: &slog::Logger,
) -> (Arc<dyn block::Backend>, ChildRegister) {
    panic!(
        "Rebuild propolis-standalone with 'crucible' feature enabled in \
           order to use the crucible block backend"
    );
}
