// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::collections::BTreeMap;
use std::num::NonZeroUsize;
use std::os::unix::fs::FileTypeExt;
use std::str::FromStr;
use std::sync::Arc;

use anyhow::Context;
use cpuid_utils::CpuidSet;
use propolis_types::CpuidIdent;
use propolis_types::CpuidValues;
use propolis_types::CpuidVendor;
use serde::{Deserialize, Serialize};

use cpuid_profile_config::*;
use propolis::block;
use propolis::hw::pci::Bdf;

use crate::cidata::build_cidata_be;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Config {
    pub main: Main,

    #[serde(default, rename = "dev")]
    pub devices: BTreeMap<String, Device>,

    #[serde(default, rename = "block_dev")]
    pub block_devs: BTreeMap<String, BlockDevice>,

    #[serde(default, rename = "cpuid")]
    pub cpuid_profiles: BTreeMap<String, CpuidProfile>,

    pub cloudinit: Option<CloudInit>,
}
impl Config {
    pub fn cpuid_profile(&self) -> Option<&CpuidProfile> {
        match self.main.cpuid_profile.as_ref() {
            Some(name) => self.cpuid_profiles.get(name),
            None => None,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Main {
    pub name: String,
    pub cpus: u8,
    pub bootrom: String,
    pub bootrom_version: Option<String>,
    pub memory: usize,
    pub use_reservoir: Option<bool>,
    pub cpuid_profile: Option<String>,
    /// Process exitcode to emit if/when instance halts
    ///
    /// Default: 0
    #[serde(default)]
    pub exit_on_halt: u8,
    /// Process exitcode to emit if/when instance reboots
    ///
    /// Default: None, does not exit on reboot
    #[serde(default)]
    pub exit_on_reboot: Option<u8>,

    /// Request bootrom override boot order using the devices specified
    pub boot_order: Option<Vec<String>>,
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
pub struct BlockOpts {
    pub block_size: Option<u32>,
    pub read_only: Option<bool>,
    pub skip_flush: Option<bool>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct BlockDevice {
    #[serde(default, rename = "type")]
    pub bdtype: String,

    #[serde(flatten)]
    pub block_opts: BlockOpts,

    #[serde(flatten, default)]
    pub options: BTreeMap<String, toml::Value>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub struct CloudInit {
    pub user_data: Option<String>,
    pub meta_data: Option<String>,
    pub network_config: Option<String>,

    // allow path-style contents as well
    pub user_data_path: Option<String>,
    pub meta_data_path: Option<String>,
    pub network_config_path: Option<String>,
}

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

#[derive(Deserialize)]
pub struct VionaDeviceParams {
    tx_copy_data: Option<bool>,
    tx_header_pad: Option<u16>,
}
impl VionaDeviceParams {
    pub fn from_opts(
        opts: &BTreeMap<String, toml::Value>,
    ) -> Result<Option<propolis::hw::virtio::viona::DeviceParams>, anyhow::Error>
    {
        use propolis::hw::virtio::viona::DeviceParams;
        let parsed: Self = opt_deser(opts)?;
        let out = if parsed.tx_copy_data.is_some()
            || parsed.tx_header_pad.is_some()
        {
            let default = DeviceParams::default();

            Some(DeviceParams {
                copy_data: parsed.tx_copy_data.unwrap_or(default.copy_data),
                header_pad: parsed.tx_header_pad.unwrap_or(default.header_pad),
            })
        } else {
            None
        };
        Ok(out)
    }
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
) -> (Arc<dyn block::Backend>, String) {
    let backend_name = dev.options.get("block_dev").unwrap().as_str().unwrap();
    let Some(be) = config.block_devs.get(backend_name) else {
        panic!("No configured block device named \"{}\"", backend_name);
    };
    let opts = block::BackendOpts {
        block_size: be.block_opts.block_size,
        read_only: be.block_opts.read_only,
        skip_flush: be.block_opts.skip_flush,
    };

    let be = match &be.bdtype as &str {
        "file" => {
            let parsed: FileConfig = opt_deser(&be.options).unwrap();

            // Check if raw device is being used and gripe if it isn't
            let meta = std::fs::metadata(&parsed.path)
                .with_context(|| {
                    format!(
                        "opening {} for block device \"{}\"",
                        parsed.path, backend_name
                    )
                })
                .expect("file device path is valid");

            if meta.file_type().is_block_device() {
                slog::warn!(log, "Block backend using standard device rather than raw";
                    "path" => &parsed.path);
            }

            block::FileBackend::create(
                &parsed.path,
                opts,
                NonZeroUsize::new(
                    parsed.workers.unwrap_or(DEFAULT_WORKER_COUNT),
                )
                .unwrap(),
            )
            .unwrap()
        }
        "crucible" => create_crucible_backend(be, opts, log),
        "crucible-mem" => create_crucible_mem_backend(be, opts, log),
        "mem-async" => {
            let parsed: MemAsyncConfig = opt_deser(&be.options).unwrap();

            block::MemAsyncBackend::create(
                parsed.size,
                opts,
                NonZeroUsize::new(
                    parsed.workers.unwrap_or(DEFAULT_WORKER_COUNT),
                )
                .unwrap(),
            )
            .unwrap()
        }
        "cloudinit" => build_cidata_be(config).unwrap(),
        _ => {
            panic!("unrecognized block dev type {}!", be.bdtype);
        }
    };
    (be, backend_name.into())
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

pub fn parse_cpuid(config: &Config) -> anyhow::Result<Option<CpuidSet>> {
    if let Some(profile) = config.cpuid_profile() {
        let vendor = match profile.vendor {
            CpuVendor::Amd => CpuidVendor::Amd,
            CpuVendor::Intel => CpuidVendor::Intel,
        };
        let mut set = CpuidSet::new(vendor);
        let entries: Vec<CpuidEntry> = profile.try_into()?;
        for entry in entries {
            let conflict = set.insert(
                CpuidIdent { leaf: entry.func, subleaf: entry.idx },
                CpuidValues::from(entry.values),
            )?;

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
    be: &BlockDevice,
    opts: block::BackendOpts,
    log: &slog::Logger,
) -> Arc<dyn block::Backend> {
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
    let block_size =
        u64::from(opts.block_size.expect("block_size is provided"));
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
    tokio::runtime::Handle::current().block_on(async move {
        block::CrucibleBackend::create(req, opts, None, None, log.clone())
            .await
            .unwrap()
    })
}

#[cfg(feature = "crucible")]
fn create_crucible_mem_backend(
    be: &BlockDevice,
    opts: block::BackendOpts,
    log: &slog::Logger,
) -> Arc<dyn block::Backend> {
    #[derive(Deserialize)]
    struct CrucibleMemConfig {
        size: u64,
    }
    let parsed: CrucibleMemConfig = opt_deser(&be.options).unwrap();

    tokio::runtime::Handle::current().block_on(async move {
        block::CrucibleBackend::create_mem(parsed.size, opts, log.clone())
            .await
            .unwrap()
    })
}

#[cfg(not(feature = "crucible"))]
fn create_crucible_backend(
    _be: &BlockDevice,
    _opts: block::BackendOpts,
    _log: &slog::Logger,
) -> Arc<dyn block::Backend> {
    panic!(
        "Rebuild propolis-standalone with 'crucible' feature enabled in \
           order to use the crucible block backend"
    );
}

#[cfg(not(feature = "crucible"))]
fn create_crucible_mem_backend(
    _be: &BlockDevice,
    _opts: block::BackendOpts,
    _log: &slog::Logger,
) -> Arc<dyn block::Backend> {
    panic!(
        "Rebuild propolis-standalone with 'crucible' feature enabled in \
           order to use the crucible-mem block backend"
    );
}
