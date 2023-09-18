// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::io::{Cursor, Write};
use std::sync::Arc;

use anyhow::{bail, Context};
use fatfs::{FileSystem, FormatVolumeOptions, FsOptions};

use propolis::block;
use propolis_standalone_config::Config;

const SECTOR_SZ: usize = 512;
const VOLUME_LABEL: [u8; 11] = *b"cidata     ";

pub(crate) fn build_cidata_be(
    config: &Config,
) -> anyhow::Result<Arc<block::InMemoryBackend>> {
    let cidata = &config
        .cloudinit
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("missing [cloudinit] config section"))?;

    let fields = [
        ("user-data", &cidata.user_data, &cidata.user_data_path),
        ("meta-data", &cidata.meta_data, &cidata.meta_data_path),
        ("network-config", &cidata.network_config, &cidata.network_config_path),
    ];
    let all_data = fields
        .into_iter()
        .map(|(name, str_data, path_data)| {
            Ok((
                name,
                match (str_data, path_data) {
                    (None, None) => vec![],
                    (None, Some(path)) => std::fs::read(path).context(
                        format!("unable to read {name} from {path}"),
                    )?,
                    (Some(data), None) => data.clone().into(),
                    (Some(_), Some(_)) => {
                        bail!("cannot provide path and string for {name}");
                    }
                },
            ))
        })
        .collect::<Result<Vec<_>, _>>()?;

    let file_sectors: usize =
        all_data.iter().map(|(_, data)| data.len().div_ceil(SECTOR_SZ)).sum();
    // vfat can hold more data than this, but we don't expect to ever need that
    // for cloud-init purposes.
    if file_sectors > 512 {
        bail!("too much vfat data: {file_sectors} > 512 sectors");
    }

    // Copying the match already done for this in Omicron:
    //
    // if we're storing < 341 KiB of clusters, the overhead is 37. With a limit
    // of 512 sectors (error check above), we can assume an overhead of 37.
    // Additionally, fatfs refuses to format a disk that is smaller than 42
    // sectors.
    let sectors = 42.max(file_sectors + 37);

    // Some tools also require that the number of sectors is a multiple of the
    // sectors-per-track. fatfs uses a default of 32 which won't evenly divide
    // sectors as we compute above generally. To fix that we simply set it to
    // match the number of sectors to make it trivially true.
    let sectors_per_track = sectors.try_into().unwrap();

    let mut disk = Cursor::new(vec![0; sectors * SECTOR_SZ]);
    fatfs::format_volume(
        &mut disk,
        FormatVolumeOptions::new()
            .bytes_per_cluster(512)
            .sectors_per_track(sectors_per_track)
            .fat_type(fatfs::FatType::Fat12)
            .volume_label(VOLUME_LABEL),
    )
    .context("error formatting FAT volume")?;

    let fs = FileSystem::new(&mut disk, FsOptions::new())?;
    let root_dir = fs.root_dir();
    for (name, data) in all_data.iter() {
        if *name == "network-config" && data.is_empty() {
            // Skip creating an empty network interfaces if nothing is provided.
            // It is not required, unlike the other files
        }
        root_dir.create_file(name)?.write_all(data)?;
    }
    drop(root_dir);
    drop(fs);

    block::InMemoryBackend::create(
        disk.into_inner(),
        block::BackendOpts {
            block_size: Some(SECTOR_SZ as u32),
            read_only: Some(true),
            ..Default::default()
        },
        std::num::NonZeroUsize::new(8).unwrap(),
    )
    .context("could not create block backend")
}
