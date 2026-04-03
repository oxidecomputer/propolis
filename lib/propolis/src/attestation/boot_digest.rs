// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use crucible::BlockIO;
use crucible::BlockIndex;
use crucible::Buffer;

use vm_attest::Measurement;

use anyhow::{anyhow, Result};
use sha2::{Digest, Sha256};
use slog::Logger;
use std::time::Instant;

pub async fn boot_disk_digest(
    vol: crucible::Volume,
    log: &Logger,
) -> Result<Measurement> {
    let vol_uuid = vol.get_uuid().await.expect("could not get volume UUID");
    let vol_size = vol.total_size().await.expect("could not get volume size");
    let block_size =
        vol.get_block_size().await.expect("could not get volume block size");

    let hash_start = Instant::now();

    let end_block = vol_size / block_size;

    // XXX(jph): Copied from the crucible scrub code.
    // This is so that we can read 128KiB of data on each read, regardless of
    // block size.
    let block_count = 131072 / block_size;

    slog::info!(
        log,
        "starting hash of volume {:?} (total_size={}, block_size={} end_block={}, block_count={})",
        vol_uuid,
        vol_size,
        block_size,
        end_block,
        block_count,
    );

    let mut hasher = Sha256::new();

    let mut offset = 0;
    while offset < end_block {
        let remaining_blocks = end_block - offset;
        let this_block_count = block_count.min(remaining_blocks);
        if this_block_count != block_count {
            slog::info!(
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

        // Read the whole disk. If a read fails, we'll retry a given number of times, but if those
        // fail, we return an error to the attestation machinery. It's unlikely that instance is
        // doing well in this case, anyway, if it's boot disk is erroring on reads.
        //
        // XXX(JPH): Crucible scrub code also inserts a delay between reads. We probably
        // don't want to do that but we'll see how this goes in production.
        let retry_count = 5;
        let mut n_retries = 0;
        loop {
            if n_retries >= retry_count {
                slog::error!(log, "failed to read from boot disk {n_retries} tries; aborting boot
                    digest hash");

                return Err(anyhow!("could not hash boot disk digest"));
            }

            let res = vol.read(block, &mut buffer).await;

            if let Err(e) = res {
                slog::error!(
                    log,
                    "read failed: {e:?}.
                offset={offset},
                this_block_cout={this_block_count},
                block_size={block_size},
                end_block={end_block}"
                );

                n_retries += 1;
            } else {
                break;
            }
        }

        hasher.update(&*buffer);

        offset += this_block_count;
    }

    let elapsed = hash_start.elapsed();
    slog::info!(
        log,
        "hash of volume {:?} took {:?} ms",
        vol_uuid,
        elapsed.as_millis()
    );

    Ok(Measurement::Sha256(hasher.finalize().into()))
}
