// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use crucible::BlockIO;
use crucible::BlockIndex;
use crucible::Buffer;

use vm_attest::Measurement;

use anyhow::{anyhow, Result};
use sha2::{Digest, Sha256};
use slog::{error, info, Logger};
use std::time::{Duration, Instant};

/// Find the SHA256 sum of a crucible volume. This should be from a read-only
/// disk; otherwise, this isn't a reliable hash.
pub async fn boot_disk_digest(
    vol: crucible::Volume,
    log: &Logger,
) -> Result<Measurement> {
    let vol_uuid = vol.get_uuid().await.expect("could not get volume UUID");
    let vol_size = vol.total_size().await.expect("could not get volume size");
    let block_size =
        vol.get_block_size().await.expect("could not get volume block size");
    let end_block = vol_size / block_size;
    let hash_start = Instant::now();

    // XXX(jph): This was copied from the crucible scrub code, so that we can
    // read 128KiB of data on each read, regardless of block size.
    let block_count = 131072 / block_size;

    info!(
        log,
        "starting hash of volume";
        "volume_id" => %vol_uuid,
        "volume_size" => vol_size,
        "block_size" => block_size,
        "end_block" => end_block,
        "block_count" => block_count,
    );

    let mut hasher = Sha256::new();
    let mut offset = 0;
    while offset < end_block {
        let remaining_blocks = end_block - offset;
        let this_block_count = block_count.min(remaining_blocks);
        if this_block_count != block_count {
            info!(
                log,
                "adjusting block_count to {} at offset {}",
                this_block_count,
                offset
            );
        }
        assert!(
            offset + this_block_count <= end_block,
            "offset={}, block_count={}, end={}",
            offset,
            this_block_count,
            end_block
        );

        let block = BlockIndex(offset);
        let mut buffer =
            Buffer::new(this_block_count as usize, block_size as usize);

        // Read the whole disk and hash it.
        //
        // If an individual read call fails, we'll retry some number of times,
        // but if that fails, just return an error to the attestation server.
        // If reads are failing on the boot disk, it's unlikely the instance is
        // doing well anyway, so there's not much to do here.
        let retry_count = 5;
        let mut n_retries = 0;
        loop {
            if n_retries >= retry_count {
                error!(
                    log,
                    "failed to read boot disk in {n_retries} tries \
		    aborting hash of boot digest"
                );

                return Err(anyhow!("could not hash boot disk digest"));
            }

            let res = vol.read(block, &mut buffer).await;

            if let Err(e) = res {
                error!(log,
                    "read failed: {e:?}";
                    "retry_count" => retry_count,
                    "io_offset" => offset,
                    "this_block_count" => this_block_count,
                    "block_size" => block_size,
                    "end_block" => end_block,
                );
                let delay = 1;
                error!(log, "will retry in {delay} secs");

                n_retries += 1;
                tokio::time::sleep(Duration::from_secs(delay)).await;
            } else {
                break;
            }
        }

        hasher.update(&*buffer);
        offset += this_block_count;
    }

    let elapsed = hash_start.elapsed();
    info!(
        log,
        "hash of volume {:?} took {:?} ms",
        vol_uuid,
        elapsed.as_millis()
    );

    Ok(Measurement::Sha256(hasher.finalize().into()))
}
