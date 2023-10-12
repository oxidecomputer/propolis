// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Abstractions for disks with a raw file backend.

use std::path::{Path, PathBuf};

use propolis_client::instance_spec::{
    components::backends::FileStorageBackend, v0::StorageBackendV0,
};
use tracing::{error, info};
use uuid::Uuid;

use crate::guest_os::GuestOsKind;

/// An RAII wrapper for a disk wrapped by a file.
#[derive(Debug)]
pub struct FileBackedDisk {
    /// The name to use for instance spec backends that refer to this disk.
    backend_name: String,

    /// The path at which the disk is stored.
    disk_path: PathBuf,

    /// The kind of guest OS image this guest contains, or `None` if the disk
    /// was not initialized from a guest OS artifact.
    guest_os: Option<GuestOsKind>,
}

impl FileBackedDisk {
    /// Creates a new file-backed disk whose initial contents are copied from
    /// the specified artifact on the host file system.
    pub(crate) fn new_from_artifact(
        backend_name: String,
        artifact_path: &impl AsRef<Path>,
        data_dir: &impl AsRef<Path>,
        guest_os: Option<GuestOsKind>,
    ) -> Result<Self, super::DiskError> {
        let mut disk_path = data_dir.as_ref().to_path_buf();
        disk_path.push(format!("{}.phd_disk", Uuid::new_v4()));
        info!(
            source = %artifact_path.as_ref().display(),
            disk_path = %disk_path.display(),
            "Copying source image to create temporary disk",
        );

        std::fs::copy(artifact_path, &disk_path)?;

        // Make sure the disk is writable (the artifact may have been
        // read-only).
        let disk_file = std::fs::File::open(&disk_path)?;
        let mut permissions = disk_file.metadata()?.permissions();

        // TODO: Clippy is upset that `set_readonly(false)` results in
        // world-writable files on UNIX-like OSes.  Suppress the lint for now
        // until someone gets around to a more specific solution.
        #[allow(clippy::permissions_set_readonly_false)]
        permissions.set_readonly(false);
        disk_file.set_permissions(permissions)?;

        Ok(Self { backend_name, disk_path, guest_os })
    }
}

impl super::DiskConfig for FileBackedDisk {
    fn backend_spec(&self) -> (String, StorageBackendV0) {
        (
            self.backend_name.clone(),
            StorageBackendV0::File(FileStorageBackend {
                path: self.disk_path.to_string_lossy().to_string(),
                readonly: false,
            }),
        )
    }

    fn guest_os(&self) -> Option<GuestOsKind> {
        self.guest_os
    }
}

impl Drop for FileBackedDisk {
    fn drop(&mut self) {
        info!(path = %self.disk_path.display(), "Deleting guest disk image");
        if let Err(e) = std::fs::remove_file(&self.disk_path) {
            error!(?e,
                   path = %self.disk_path.display(),
                   "Failed to delete guest disk image");
        }
    }
}
