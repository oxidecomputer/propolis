// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Tools for creating a FAT volume that can be used as the contents of a VM
//! disk.

use std::io::{Cursor, Write};

use anyhow::Context;
use fatfs::{FileSystem, FormatVolumeOptions, FsOptions};
use newtype_derive::*;
use thiserror::Error;

/// The maximum size of disk this module can create. This is fixed to put an
/// upper bound on the overhead the FAT filesystem requires.
///
/// This value must be less than or equal to 2,091,008 (4084 * 512). See the
/// docs for [`overhead_sectors`].
const MAX_DISK_SIZE_BYTES: usize = 1 << 20;

/// The size of a sector in this module's produced volumes.
///
/// This module assumes that each FAT cluster is one sector in size.
const BYTES_PER_SECTOR: usize = 512;

/// The size of a directory entry, given by the FAT specification.
const BYTES_PER_DIRECTORY_ENTRY: usize = 32;

/// The number of directory entries in the filesystem's root directory region.
const ROOT_DIRECTORY_ENTRIES: usize = 512;

/// The number of allocation tables that can be found in each FAT volume. This
/// is the default value used by the `fatfs` crate.
const TABLES_PER_VOLUME: usize = 2;

/// A number of disk sectors.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
struct Sectors(usize);

NewtypeAdd! { () struct Sectors(usize); }
NewtypeAddAssign! { () struct Sectors(usize); }
NewtypeSub! { () struct Sectors(usize); }
NewtypeSubAssign! { () struct Sectors(usize); }
NewtypeMul! { () struct Sectors(usize); }
NewtypeMul! { (usize) struct Sectors(usize); }

impl Sectors {
    /// Yields the number of sectors needed to hold the supplied quantity of
    /// bytes.
    const fn needed_for_bytes(bytes: usize) -> Self {
        Self(bytes.div_ceil(BYTES_PER_SECTOR))
    }
}

/// Computes the number of sectors to reserve for overhead in this module's FAT
/// volumes.
///
/// FAT volumes consist of four regions:
///
/// 1. A reserved region for the BIOS parameter block (BPB)
/// 2. The file allocation tables themselves
/// 3. A set of root directory entries (FAT12/FAT16 only)
/// 4. File and directory data
///
/// The file and directory data is divided into clusters of one or more sectors.
/// The cluster size is given in the FAT metadata in the BPB; in this module
/// each cluster contains just one sector.
///
/// The specific format of a FAT volume (FAT12 vs. FAT16 vs. FAT32) depends on
/// the number of clusters in the file and directory data region:
///
/// - 0 <= clusters <= 4084: FAT12
/// - 4085 <= clusters <= 65524: FAT16
/// - clusters >= 65525: FAT32
///
/// There is a small catch-22 here: a volume's format depends on the number of
/// clusters it has, but the number of clusters in the volume depends on the
/// size of the filesystem metadata, which depends on the volume's format.
/// This module avoids the problem by requiring that the total volume size is
/// less than or equal to 4,084 sectors. The number of clusters will never be
/// greater than this, so the filesystem will always be FAT12, which makes it
/// possible to compute its overhead size.
fn overhead_sectors() -> Sectors {
    let dir_entry_bytes = ROOT_DIRECTORY_ENTRIES * BYTES_PER_DIRECTORY_ENTRY;
    let dir_entry_sectors = Sectors::needed_for_bytes(dir_entry_bytes);

    // To compute the size of the FAT, conservatively assume that all the
    // sectors on the disk are in addressable clusters, figure out how many
    // bytes that would take, and convert to sectors. (In practice, some of the
    // sectors are used for overhead and won't have entries in the FAT, but this
    // gives an upper bound.)
    let max_sectors = Sectors::needed_for_bytes(MAX_DISK_SIZE_BYTES);

    // FAT12 tables use 12 bits per cluster entry.
    let cluster_bits = max_sectors.0 * 12;
    let cluster_bytes = cluster_bits.div_ceil(8);
    let sectors_per_table = Sectors::needed_for_bytes(cluster_bytes);

    // The total overhead size is one sector (for the BPB) plus the sectors
    // needed for regions 2 and 3 (the tables themselves and the root directory
    // entries). Note that there are multiple tables per volume!
    Sectors(1) + (sectors_per_table * TABLES_PER_VOLUME) + dir_entry_sectors
}

/// Yields the number of sectors in this module's FAT volumes that can be used
/// by files.
fn total_usable_sectors() -> Sectors {
    Sectors::needed_for_bytes(MAX_DISK_SIZE_BYTES) - overhead_sectors()
}

#[derive(Clone, Debug)]
struct File {
    path: String,
    contents: Vec<u8>,
}

#[derive(Debug, Error)]
pub enum Error {
    #[error(
        "insufficient space for new file: {0} sectors required, {1} available"
    )]
    NoSpace(usize, usize),
}

/// Builds a collection of files that can be extruded as an array of bytes
/// containing a FAT filesystem with the collected files.
#[derive(Clone, Default, Debug)]
pub struct FatFilesystem {
    files: Vec<File>,
    sectors_remaining: Sectors,
}

impl FatFilesystem {
    /// Creates a new file collection.
    pub fn new() -> Self {
        Self { files: vec![], sectors_remaining: total_usable_sectors() }
    }

    /// Adds a file with the supplied `contents` that will appear in the
    /// resulting file system in the supplied `path`.
    ///
    /// N.B. `path` is interpreted relative to the root of the file system and
    ///      should not have a leading '/'.
    pub fn add_file_from_str(
        &mut self,
        path: &str,
        contents: &str,
    ) -> Result<(), Error> {
        let bytes = contents.as_bytes();
        let sectors_needed = Sectors::needed_for_bytes(bytes.len());
        if sectors_needed > self.sectors_remaining {
            Err(Error::NoSpace(sectors_needed.0, self.sectors_remaining.0))
        } else {
            self.files
                .push(File { path: path.to_owned(), contents: bytes.to_vec() });
            self.sectors_remaining -= sectors_needed;
            Ok(())
        }
    }

    pub fn as_bytes(&self) -> anyhow::Result<Vec<u8>> {
        let file_sectors: usize = self
            .files
            .iter()
            .map(|f| Sectors::needed_for_bytes(f.contents.len()).0)
            .sum();

        assert!(file_sectors <= total_usable_sectors().0);

        // `fatfs` requires that the output volume has at least 42 sectors, no
        // matter what.
        let sectors = 42.max(file_sectors + overhead_sectors().0);

        assert!(sectors <= Sectors::needed_for_bytes(MAX_DISK_SIZE_BYTES).0);

        // Some guest software requires that a FAT disk's sector count be a
        // multiple of the sectors-per-track value in its BPB. Trivially enforce
        // this by ensuring there's one track containing all the sectors.
        let sectors_per_track: u16 = sectors.try_into().map_err(|_| {
            anyhow::anyhow!(
                "disk has {} sectors, which is too many for one FAT track",
                sectors
            )
        })?;

        let mut disk = Cursor::new(vec![0; sectors * BYTES_PER_SECTOR]);
        fatfs::format_volume(
            &mut disk,
            FormatVolumeOptions::new()
                .bytes_per_sector(BYTES_PER_SECTOR.try_into().unwrap())
                .bytes_per_cluster(BYTES_PER_SECTOR.try_into().unwrap())
                .sectors_per_track(sectors_per_track)
                .fat_type(fatfs::FatType::Fat12)
                .volume_label(*b"phd        "),
        )
        .context("formatting FAT volume")?;

        {
            let fs = FileSystem::new(&mut disk, FsOptions::new())
                .context("creating FAT filesystem")?;

            let root_dir = fs.root_dir();
            for file in &self.files {
                root_dir
                    .create_file(&file.path)
                    .with_context(|| format!("creating file {}", file.path))?
                    .write_all(file.contents.as_slice())
                    .with_context(|| {
                        format!("writing contents of file {}", file.path)
                    })?;
            }
        }

        Ok(disk.into_inner())
    }
}
