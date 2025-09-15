// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Abstractions for disks with a raw file backend.

use camino::{Utf8Path, Utf8PathBuf};
use propolis_client::instance_spec::{ComponentV0, FileStorageBackend};
use std::num::NonZeroUsize;
use tracing::{debug, error, warn};
use uuid::Uuid;

use crate::{
    disk::DeviceName, guest_os::GuestOsKind, zfs::ClonedFile as ZfsClonedFile,
};

/// Describes the method used to create the backing file for a file-backed disk.
#[derive(Debug)]
enum BackingFile {
    /// The disk is a ZFS clone of the original artifact.
    Zfs(ZfsClonedFile),

    /// The disk is a hard copy of the original artifact.
    HardCopy(Utf8PathBuf),
}

impl BackingFile {
    /// Creates a new backing file from the artifact with the supplied
    /// `artifact_path`. If possible, this routine will create a ZFS clone of
    /// the dataset containing the file; otherwise it will fall back to creating
    /// a hard copy of the original artifact.
    fn create_from_source(
        artifact_path: &Utf8Path,
        data_dir: &Utf8Path,
    ) -> anyhow::Result<Self> {
        match ZfsClonedFile::create_from_path(artifact_path) {
            Ok(file) => return Ok(Self::Zfs(file)),
            Err(error) => warn!(
                %artifact_path,
                %error,
                "failed to make ZFS clone of backing artifact, will copy it"
            ),
        }

        let mut disk_path = data_dir.to_path_buf();
        disk_path.push(format!("{}.phd_disk", Uuid::new_v4()));
        debug!(
            source = %artifact_path,
            disk_path = %disk_path,
            "Copying source image to create temporary disk",
        );

        std::fs::copy(artifact_path, &disk_path)?;
        Ok(Self::HardCopy(disk_path))
    }

    /// Yields the path to this disk's backing file.
    fn path(&self) -> Utf8PathBuf {
        match self {
            BackingFile::Zfs(zfs) => zfs.path(),
            BackingFile::HardCopy(path) => path.clone(),
        }
    }
}

impl Drop for BackingFile {
    fn drop(&mut self) {
        // ZFS clones are cleaned up by their own drop impls.
        if let BackingFile::HardCopy(ref path) = self {
            debug!(%path, "deleting hard copy of guest disk image");
            if let Err(e) = std::fs::remove_file(path) {
                error!(
                    ?e,
                    %path,
                    "failed to delete hard copy of guest disk image"
                );
            }
        }
    }
}

/// An RAII wrapper for a disk wrapped by a file.
#[derive(Debug)]
pub struct FileBackedDisk {
    /// The name to use for instance spec backends that refer to this disk.
    device_name: DeviceName,

    /// The backing file for this disk.
    file: BackingFile,

    /// The kind of guest OS image this guest contains, or `None` if the disk
    /// was not initialized from a guest OS artifact.
    guest_os: Option<GuestOsKind>,
}

impl FileBackedDisk {
    /// Creates a new file-backed disk whose initial contents are copied from
    /// the specified artifact on the host file system.
    pub(crate) fn new_from_artifact(
        device_name: DeviceName,
        artifact_path: &impl AsRef<Utf8Path>,
        data_dir: &impl AsRef<Utf8Path>,
        guest_os: Option<GuestOsKind>,
    ) -> Result<Self, super::DiskError> {
        let artifact = BackingFile::create_from_source(
            artifact_path.as_ref(),
            data_dir.as_ref(),
        )?;

        // Make sure the disk is writable (the artifact may have been
        // read-only).
        let disk_file = std::fs::File::open(artifact.path())?;
        let mut permissions = disk_file.metadata()?.permissions();

        // TODO: Clippy is upset that `set_readonly(false)` results in
        // world-writable files on UNIX-like OSes.  Suppress the lint for now
        // until someone gets around to a more specific solution.
        #[allow(clippy::permissions_set_readonly_false)]
        permissions.set_readonly(false);
        disk_file.set_permissions(permissions)?;

        Ok(Self { device_name, file: artifact, guest_os })
    }
}

impl super::DiskConfig for FileBackedDisk {
    fn device_name(&self) -> &DeviceName {
        &self.device_name
    }

    fn backend_spec(&self) -> ComponentV0 {
        ComponentV0::FileStorageBackend(FileStorageBackend {
            path: self.file.path().to_string(),
            readonly: false,
            block_size: 512,
            workers: Some(NonZeroUsize::new(8).unwrap()),
        })
    }

    fn guest_os(&self) -> Option<GuestOsKind> {
        self.guest_os
    }
}
