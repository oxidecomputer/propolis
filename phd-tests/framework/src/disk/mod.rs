//! Routines for creating and managing guest disks.
//!
//! Test cases create disks using the [`DiskFactory`] in their test contexts.
//! They can then pass these disks to the VM factory to connect them to a
//! specific guest VM. See the documentation for [`DiskFactory::create_disk`]
//! for more information.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use propolis_client::instance_spec::StorageBackend;
use thiserror::Error;

use crate::{artifacts::ArtifactStore, guest_os::GuestOsKind};

use self::file::FileBackedDisk;

mod file;

/// Errors that can arise while working with disks.
#[derive(Debug, Error)]
pub enum DiskError {
    #[error("Could not find source artifact {0}")]
    ArtifactNotFound(String),

    #[error(transparent)]
    IoError(#[from] std::io::Error),
}

/// A trait for functions exposed by all disk backends (files, Crucible, etc.).
trait DiskWrapper: std::fmt::Debug {
    /// Yields the backend spec for this disk's storage backend.
    fn backend_spec(&self) -> StorageBackend;
}

/// An RAII wrapper around a guest disks and all the host objects needed to
/// support it. When dropped, destroys the resources associated with the disk
/// (backing files, file-serving processes, etc.).
///
/// New guest disks created by a [`DiskFactory`] are wrapped in an `Arc` so that
/// test cases can decide whether to share them or move them into a new VM. See
/// the documentation for [`DiskFactory::create_disk`] for more information.
#[derive(Debug)]
pub struct GuestDisk {
    backend: Box<dyn DiskWrapper>,
    guest_os: Option<GuestOsKind>,
}

impl GuestDisk {
    /// Generates the backend spec that should be inserted into a VM's instance
    /// spec to refer to this disk.
    pub(crate) fn backend_spec(&self) -> StorageBackend {
        self.backend.backend_spec()
    }

    /// If this disk was sourced from a guest OS image, returns that image type.
    /// If the disk was not sourced from a known guest OS artifact, returns
    /// `None`.
    pub(crate) fn guest_os(&self) -> Option<GuestOsKind> {
        self.guest_os
    }
}

/// The possible sources for a disk's initial data.
#[derive(Debug)]
pub enum DiskSource<'a> {
    /// A disk backed by the guest image artifact with the supplied key.
    Artifact(&'a str),
}

/// The possible kinds of backends a disk can have.
#[derive(Clone, Copy, Debug)]
pub enum DiskBackend {
    /// Back this disk with a file on the local file system.
    File,
}

/// A factory that provides tests with the means to create a disk they can
/// attach to a guest VM.
pub struct DiskFactory<'a> {
    /// The directory in which disk files should be stored.
    storage_dir: PathBuf,

    /// A reference to the artifact store to use to look up guest OS artifacts
    /// when those are used as a disk source.
    artifact_store: &'a ArtifactStore,
}

impl<'a> DiskFactory<'a> {
    /// Creates a new disk factory. The disks this factory generates will store
    /// their data in `storage_dir` and will look up guest OS images in the
    /// supplied `artifact_store`.
    pub fn new(
        storage_dir: &impl AsRef<Path>,
        artifact_store: &'a ArtifactStore,
    ) -> Self {
        Self { storage_dir: storage_dir.as_ref().to_path_buf(), artifact_store }
    }
}

impl DiskFactory<'_> {
    /// Creates a new [`GuestDisk`] whose initial contents are described by
    /// `source` and that uses the supplied kind of `backend`.
    ///
    /// The returned disk is wrapped in an `Arc` that can be passed to
    /// `ConfigRequest` routines that add disks to a VM's configuration. This
    /// enables disks to be managed in two ways:
    ///
    /// 1. Tests that don't need a disk's resources to outlive a VM can simply
    ///    move the disk reference into the VM config (which will move the
    ///    reference to the VM). In this way the disk is destroyed when its test
    ///    VM goes away.
    /// 2. Tests that want to preserve or reuse a disk after its VM stops can
    ///    instead clone the reference into the VM and reuse the source
    ///    reference later in the test. This can be used to, say, launch a VM,
    ///    destroy it, and attach the same disk to another VM to verify that
    ///    changes to it are persisted.
    ///
    /// N.B. The `GuestDisk` objects this function creates take no special care
    ///      to ensure that they can be used safely by multiple VMs at the same
    ///      time. If multiple VMs do use a single set of backend resources, the
    ///      resulting behavior will depend on the chosen backend's semantics
    ///      and the way the Propolis backend implementations interact with the
    ///      disk.
    pub fn create_disk(
        &self,
        source: DiskSource,
        _backend: DiskBackend,
    ) -> Result<Arc<GuestDisk>, DiskError> {
        let DiskSource::Artifact(artifact_name) = source;
        let artifact_path = self
            .artifact_store
            .get_guest_image_path_by_name(artifact_name)
            .ok_or_else(|| {
                DiskError::ArtifactNotFound(artifact_name.to_string())
            })?;

        let guest_os = self
            .artifact_store
            .get_guest_os_kind_by_name(artifact_name)
            .ok_or_else(|| {
                DiskError::ArtifactNotFound(artifact_name.to_string())
            })?;

        // disk backend
        let disk = FileBackedDisk::new_from_artifact(
            &artifact_path,
            &self.storage_dir,
        )?;

        Ok(Arc::new(GuestDisk {
            backend: disk as Box<dyn DiskWrapper>,
            guest_os: Some(guest_os),
        }))
    }
}
