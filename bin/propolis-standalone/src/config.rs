// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::collections::BTreeMap;
use std::num::NonZeroUsize;
use std::os::unix::fs::FileTypeExt;
use std::str::FromStr;
use std::sync::Arc;

use anyhow::Context;
use serde::Deserialize;

use propolis::block;
use propolis::cpuid;
use propolis::hw::pci::Bdf;
use propolis::inventory::ChildRegister;

use crate::cidata::build_cidata_be;
pub use propolis_standalone_config::{Config, SnapshotTag};
use propolis_standalone_config::{CpuVendor, CpuidEntry, Device};

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

            // Check if raw device is being used and gripe if it isn't
            let meta = std::fs::metadata(&parsed.path)
                .expect("file device path is valid");
            if meta.file_type().is_block_device() {
                slog::warn!(log, "Block backend using standard device rather than raw";
                    "path" => &parsed.path);
            }

            let be = block::FileBackend::create(
                &parsed.path,
                opts,
                NonZeroUsize::new(
                    parsed.workers.unwrap_or(DEFAULT_WORKER_COUNT),
                )
                .unwrap(),
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
        "cloudinit" => {
            let be = build_cidata_be(config).unwrap();
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

pub fn parse_cpuid(config: &Config) -> anyhow::Result<Option<cpuid::Set>> {
    if let Some(profile) = config.cpuid_profile() {
        let vendor = match profile.vendor {
            CpuVendor::Amd => cpuid::VendorKind::Amd,
            CpuVendor::Intel => cpuid::VendorKind::Intel,
        };
        let mut set = cpuid::Set::new(vendor);
        let entries: Vec<CpuidEntry> = profile.try_into()?;
        for entry in entries {
            let conflict = set.insert(
                cpuid::Ident(entry.func, entry.idx),
                cpuid::Entry::from(entry.values),
            );

            if conflict.is_some() {
                anyhow::bail!(
                    "conflicing entry at func:{:#?} idx:{:#?}",
                    entry.func,
                    entry.idx
                )
            }
        }
        Ok(Some(set))
    } else {
        Ok(None)
    }
}

#[cfg(feature = "crucible")]
fn create_crucible_backend(
    be: &propolis_standalone_config::BlockDevice,
    opts: block::BackendOpts,
    log: &slog::Logger,
) -> (Arc<dyn block::Backend>, ChildRegister) {
    use slog::info;
    use std::net::SocketAddr;
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
    let block_size = opts.block_size.expect("block_size is provided") as u64;
    let read_only = opts.read_only.unwrap_or(false);

    #[derive(Deserialize)]
    struct CrucibleConfig {
        blocks_per_extent: u64,
        extent_count: u32,
        upstairs_id: Option<String>,
        targets: [String; 3],

        // This needs to increase monotonically with each successive connection
        // to the downstairs. As a hack, you can set it to the current system
        // time, and this will usually give us a newer generation than the last
        // connection. NEVER do this in prod EVER.
        generation: u64,

        lossy: Option<bool>,
        flush_timeout: Option<f32>,
        encryption_key: Option<String>,
        cert_pem: Option<String>,
        key_pem: Option<String>,
        root_cert_pem: Option<String>,
        control_addr: Option<String>,
    }
    let parsed: CrucibleConfig = opt_deser(&be.options).unwrap();

    // Parse a UUID, or generate a random one if none is specified.
    // Reasonable in something primarily used for testing like
    // propolis-standalone, but you wouldn't want to do this in
    // prod.
    let upstairs_id = if let Some(val) = parsed.upstairs_id {
        Uuid::parse_str(&val).expect("upstairs_id is valid uuid")
    } else {
        Uuid::new_v4()
    };

    let target = parsed
        .targets
        .iter()
        .map(|val| val.parse::<SocketAddr>())
        .collect::<Result<Vec<_>, _>>()
        .expect("targets contains valid socket addresses");

    let control = parsed.control_addr.map(|val| {
        val.parse::<SocketAddr>().expect("control_addr is valid socket addr")
    });

    let req = crucible_client_types::VolumeConstructionRequest::Region {
        block_size,
        blocks_per_extent: parsed.blocks_per_extent,
        extent_count: parsed.extent_count,
        opts: crucible_client_types::CrucibleOpts {
            id: upstairs_id,
            target,
            lossy: parsed.lossy.unwrap_or(false),
            flush_timeout: parsed.flush_timeout,
            key: parsed.encryption_key,
            cert_pem: parsed.cert_pem,
            key_pem: parsed.key_pem,
            root_cert_pem: parsed.root_cert_pem,
            control,
            read_only,
        },
        gen: parsed.generation,
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
