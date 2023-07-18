// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Describes a server config which may be parsed from a TOML file.

use std::num::NonZeroUsize;
use std::sync::Arc;

use propolis::block;
use propolis::inventory;
pub use propolis_server_config::*;

pub fn create_backend_for_block(
    config: &Config,
    name: &str,
    log: slog::Logger,
) -> Result<(Arc<dyn block::Backend>, inventory::ChildRegister), ParseError> {
    let entry = config.block_devs.get(name).ok_or_else(|| {
        ParseError::KeyNotFound(name.to_string(), "block_dev".to_string())
    })?;
    blockdev_backend(entry, log)
}

fn blockdev_backend(
    dev: &BlockDevice,
    log: slog::Logger,
) -> Result<(Arc<dyn block::Backend>, inventory::ChildRegister), ParseError> {
    match &dev.bdtype as &str {
        "file" => {
            let path = dev
                .options
                .get("path")
                .ok_or_else(|| {
                    ParseError::KeyNotFound(
                        "path".to_string(),
                        "options".to_string(),
                    )
                })?
                .as_str()
                .ok_or_else(|| {
                    ParseError::AsError(
                        "path".to_string(),
                        "as_str".to_string(),
                    )
                })?;

            let readonly: bool = || -> Option<bool> {
                match dev.options.get("readonly") {
                    Some(toml::Value::Boolean(read_only)) => Some(*read_only),
                    Some(toml::Value::String(v)) => v.parse().ok(),
                    _ => None,
                }
            }()
            .unwrap_or(false);
            let nworkers = NonZeroUsize::new(8).unwrap();
            let be = propolis::block::FileBackend::create(
                path, readonly, nworkers, log,
            )?;
            let child =
                inventory::ChildRegister::new(&be, Some(path.to_string()));

            Ok((be, child))
        }
        _ => {
            panic!("unrecognized block dev type {}!", dev.bdtype);
        }
    }
}

#[cfg(not(feature = "omicron-build"))]
pub fn reservoir_decide(log: &slog::Logger) -> bool {
    // Automatically enable use of the memory reservoir (rather than transient
    // allocations) for guest memory if it meets some arbitrary size threshold.
    const RESERVOIR_THRESH_MB: usize = 512;

    match propolis::vmm::query_reservoir() {
        Err(e) => {
            slog::error!(log, "could not query reservoir {:?}", e);
            false
        }
        Ok(size) => {
            let size_in_play =
                (size.vrq_alloc_sz + size.vrq_free_sz) / (1024 * 1024);
            if size_in_play > RESERVOIR_THRESH_MB {
                slog::info!(
                    log,
                    "allocating from reservoir ({}MiB) for guest memory",
                    size_in_play
                );
                true
            } else {
                slog::info!(
                    log,
                    "reservoir too small ({}MiB) to use for guest memory",
                    size_in_play
                );
                false
            }
        }
    }
}

#[cfg(feature = "omicron-build")]
pub fn reservoir_decide(_log: &slog::Logger) -> bool {
    // Always use the reservoir in production
    true
}
