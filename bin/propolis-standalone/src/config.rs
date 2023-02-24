use std::str::FromStr;
use std::sync::Arc;
use std::time::SystemTime;
use std::{num::NonZeroUsize, time::Instant};

use anyhow::Context;

use propolis::block;
use propolis::hw::pci::Bdf;
use propolis::inventory::ChildRegister;

use propolis_standalone_config::Device;
pub use propolis_standalone_config::{Config, SnapshotTag};

use slog::info;
use uuid::Uuid;

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

            let readonly = (match be.options.get("readonly") {
                Some(toml::Value::Boolean(read_only)) => Some(*read_only),
                Some(toml::Value::String(v)) => v.parse().ok(),
                _ => None,
            })
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
        "crucible" => {
            info!(log, "Building a crucible VolumeConstructionRequest from options {:?}", be.options);

            // No defaults on here because we really shouldn't try and guess
            // what block size the downstairs is using. A lot of things
            // default to 512, but it's best not to assume it'll always be
            // that way.
            let block_size =
                be.options.get("block_size").unwrap().as_integer().unwrap()
                    as u64;

            let blocks_per_extent = be
                .options
                .get("blocks_per_extent")
                .unwrap()
                .as_integer()
                .unwrap() as u64;

            let extent_count =
                be.options.get("extent_count").unwrap().as_integer().unwrap()
                    as u32;

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

            let lossy = be
                .options
                .get("lossy")
                .map(|x| x.as_bool().unwrap())
                .unwrap_or(false);

            let flush_timeout = be
                .options
                .get("flush_timeout")
                .map(|x| x.as_integer().unwrap() as u32);

            let key = be
                .options
                .get("encryption_key")
                .map(|x| x.as_str().unwrap().to_string());

            let cert_pem = be
                .options
                .get("cert_pem")
                .map(|x| x.as_str().unwrap().to_string());

            let key_pem = be
                .options
                .get("key_pem")
                .map(|x| x.as_str().unwrap().to_string());

            let root_cert_pem = be
                .options
                .get("root_cert_pem")
                .map(|x| x.as_str().unwrap().to_string());

            let control_addr =
                be.options.get("control_addr").map(|target_val| {
                    target_val.as_str().unwrap().parse().unwrap()
                });

            let read_only = be
                .options
                .get("read_only")
                .map(|x| x.as_bool().unwrap())
                .unwrap_or(false);

            // This needs to increase monotonically with each successive
            // connection to the downstairs. As a hack, you can set it to
            // the current system time, and this will usually give us a newer
            // generation than the last connection. NEVER do this in prod
            // EVER.
            let generation = be
                .options
                .get("generation")
                .unwrap()
                .as_integer()
                .unwrap() as u64;

            let req =
                crucible_client_types::VolumeConstructionRequest::Region {
                    block_size: block_size,
                    blocks_per_extent: blocks_per_extent,
                    extent_count: extent_count,
                    opts: crucible_client_types::CrucibleOpts {
                        id: uuid,
                        target: targets,
                        lossy: lossy,
                        flush_timeout: flush_timeout,
                        key: key,
                        cert_pem: cert_pem,
                        key_pem: key_pem,
                        root_cert_pem: root_cert_pem,
                        control: control_addr,
                        read_only: read_only,
                    },
                    gen: generation,
                };
            info!(log, "Creating Crucible disk from request {:?}", req);
            // QUESTION: is producer_registry: None correct here?
            let be =
                block::CrucibleBackend::create(req.clone(), read_only, None)
                    .unwrap();
            let creg = ChildRegister::new(
                &be,
                Some(be.get_uuid().unwrap().to_string()),
            );
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
