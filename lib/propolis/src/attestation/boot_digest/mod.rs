// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use vm_attest::Measurement;

use anyhow::Result;
use slog::Logger;

#[cfg(feature = "crucible")]
mod crucible;

#[derive(Debug)]
pub enum Backend {
    #[cfg(feature = "crucible")]
    Crucible(::crucible::Volume),
}

pub async fn compute(backend: Backend, log: &Logger) -> Result<Measurement> {
    slog::info!(log, "computing disk digest for {backend:?}");

    match backend {
        #[cfg(feature = "crucible")]
        Backend::Crucible(vol) => {
            // TODO: load-bearing sleep: we have a Crucible volume, but we can
            // be here and chomping at the bit to get a digest calculation
            // started well before the volume has been activated; in
            // `propolis-server` we need to wait for at least a subsequent
            // instance start. Similar to the scrub task for Crucible disks,
            // delay some number of seconds in the hopes that activation is done
            // promptly.
            //
            // This should be replaced by awaiting for some kind of actual
            // "activated" signal.
            //
            // see #1078
            tokio::time::sleep(std::time::Duration::from_secs(10)).await;

            crucible::boot_disk_digest(vol, log).await
        }
    }
}
