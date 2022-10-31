//! Abstractions for disks with a raw file backend.

use std::path::{Path, PathBuf};

use propolis_client::instance_spec::{StorageBackend, StorageBackendKind};
use tracing::{error, info};
use uuid::Uuid;

/// An RAII wrapper for a disk wrapped by a file.
#[derive(Debug)]
pub(crate) struct FileBackedDisk {
    /// The path at which the disk is stored.
    disk_path: PathBuf,
}

impl FileBackedDisk {
    /// Creates a new file-backed disk whose initial contents are copied from
    /// the specified artifact on the host file system.
    pub(crate) fn new_from_artifact(
        artifact_path: &impl AsRef<Path>,
        data_dir: &impl AsRef<Path>,
    ) -> Result<Box<Self>, super::DiskError> {
        let mut disk_path = data_dir.as_ref().to_path_buf();
        disk_path.push(format!("{}.phd_disk", Uuid::new_v4()));
        info!(
            source = %artifact_path.as_ref().display(),
            disk_path = %disk_path.display(),
            "Copying source image to create temporary disk",
        );

        std::fs::copy(&artifact_path, &disk_path)?;

        // Make sure the disk is writable (the artifact may have been
        // read-only).
        let disk_file = std::fs::File::open(&disk_path)?;
        let mut permissions = disk_file.metadata()?.permissions();
        permissions.set_readonly(false);
        disk_file.set_permissions(permissions)?;

        Ok(Box::new(Self { disk_path }))
    }
}

impl super::DiskWrapper for FileBackedDisk {
    fn backend_spec(&self) -> StorageBackend {
        StorageBackend {
            kind: StorageBackendKind::File {
                path: self.disk_path.to_string_lossy().to_string(),
            },
            readonly: false,
        }
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