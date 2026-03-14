// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::sync::Arc;
use std::time::Instant;

use anyhow::Result;
use crucible::BlockIO;
use crucible::BlockIndex;
use crucible::Buffer;
use sha2::{Digest, Sha256};
use slog::Logger;

use propolis::block;

pub fn get_crucible_volume(
    backend: &Arc<dyn block::Backend>,
) -> crucible::Volume {
    let crucible_be = backend
        .as_any()
        .downcast_ref::<block::CrucibleBackend>()
        .expect("backend is not a CrucibleBackend");
    crucible_be.clone_volume()
}

// TODO: start delay?
pub async fn hash_crucible_disk(
    vol: crucible::Volume,
    log: &Logger,
) -> Result<String> {
    let vol_uuid = vol.get_uuid().await.expect("could not get volume UUID");
    let vol_size = vol.total_size().await.expect("could not get volume size");
    let block_size =
        vol.get_block_size().await.expect("could not get volume block size");

    slog::info!(
        log,
        "starting hash of volume {:?} \
         (total_size={}, block_size={})",
        vol_uuid,
        vol_size,
        block_size
    );
    let hash_start = Instant::now();

    let end = vol_size / block_size;

    // TODO: it's jank, apparently
    // copying this I/O sizing from the crucible scrub code
    let block_count = 131072 / block_size;

    let mut hasher = Sha256::new();

    let mut offset = 0;
    while offset < end {
        let remaining = end - offset;
        let this_block_count = block_count.min(remaining);
        if this_block_count != block_count {
            slog::info!(
                log,
                "adjusting block_count to {} at offset {}",
                this_block_count,
                offset
            );
        }
        assert!(
            offset + this_block_count <= end,
            "offset={}, block_count={}, end={}",
            offset,
            this_block_count,
            end
        );

        let block = BlockIndex(offset);
        let mut buffer =
            Buffer::new(this_block_count as usize, block_size as usize);
        vol.read(block, &mut buffer).await.expect("could not read volume");

        hasher.update(&*buffer);

        offset += this_block_count;

        // TODO: delay?
    }

    let elapsed = hash_start.elapsed();
    slog::info!(
        log,
        "hash of vol {:?} took {:?} secs",
        vol_uuid,
        elapsed.as_secs()
    );

    let hash = hasher.finalize();
    Ok(format!("{:x}", hash))
}
