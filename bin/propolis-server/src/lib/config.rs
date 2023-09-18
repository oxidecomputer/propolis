// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Describes a server config which may be parsed from a TOML file.

pub use propolis_server_config::*;

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
