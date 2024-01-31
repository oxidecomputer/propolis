// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Abstractions for disks with a raw file backend.

use camino::Utf8Path;
use propolis_client::types::{FileStorageBackend, StorageBackendV0};

use crate::{guest_os::GuestOsKind, zfs::ClonedFile as ZfsClonedFile};

/// An RAII wrapper for a disk wrapped by a file.
#[derive(Debug)]
pub struct FileBackedDisk {
    /// The name to use for instance spec backends that refer to this disk.
    backend_name: String,

    /// The backing file for this disk.
    file: ZfsClonedFile,

    /// The kind of guest OS image this guest contains, or `None` if the disk
    /// was not initialized from a guest OS artifact.
    guest_os: Option<GuestOsKind>,
}

impl FileBackedDisk {
    /// Creates a new file-backed disk whose initial contents are copied from
    /// the specified artifact on the host file system.
    pub(crate) fn new_from_artifact(
        backend_name: String,
        artifact_path: &impl AsRef<Utf8Path>,
        guest_os: Option<GuestOsKind>,
    ) -> Result<Self, super::DiskError> {
        let artifact = ZfsClonedFile::create_from_path(artifact_path.as_ref())?;
        let disk_path = artifact.path().into_std_path_buf();

        // Make sure the disk is writable (the artifact may have been
        // read-only).
        let disk_file = std::fs::File::open(disk_path)?;
        let mut permissions = disk_file.metadata()?.permissions();

        // TODO: Clippy is upset that `set_readonly(false)` results in
        // world-writable files on UNIX-like OSes.  Suppress the lint for now
        // until someone gets around to a more specific solution.
        #[allow(clippy::permissions_set_readonly_false)]
        permissions.set_readonly(false);
        disk_file.set_permissions(permissions)?;

        Ok(Self { backend_name, file: artifact, guest_os })
    }
}

impl super::DiskConfig for FileBackedDisk {
    fn backend_spec(&self) -> (String, StorageBackendV0) {
        (
            self.backend_name.clone(),
            StorageBackendV0::File(FileStorageBackend {
                path: self.file.path().to_string(),
                readonly: false,
            }),
        )
    }

    fn guest_os(&self) -> Option<GuestOsKind> {
        self.guest_os
    }
}
