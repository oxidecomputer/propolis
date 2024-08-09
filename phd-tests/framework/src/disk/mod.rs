// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Routines for creating and managing guest disks.
//!
//! Test cases create disks using the [`DiskFactory`] in their test contexts.
//! They can then pass these disks to the VM factory to connect them to a
//! specific guest VM.

use std::sync::Arc;

use anyhow::Context;
use camino::{Utf8Path, Utf8PathBuf};
use in_memory::InMemoryDisk;
use propolis_client::types::StorageBackendV0;
use thiserror::Error;

use crate::{
    artifacts::ArtifactStore,
    guest_os::GuestOsKind,
    port_allocator::{PortAllocator, PortAllocatorError},
    server_log_mode::ServerLogMode,
};

use self::{crucible::CrucibleDisk, file::FileBackedDisk};

pub mod crucible;
pub mod fat;
mod file;
pub mod in_memory;

/// Errors that can arise while working with disks.
#[derive(Debug, Error)]
pub enum DiskError {
    #[error("Disk factory has no Crucible downstairs artifact")]
    NoCrucibleDownstairs,

    #[error("can't create a {disk_type} disk from source of type {src}")]
    SourceNotSupported { disk_type: &'static str, src: &'static str },

    #[error(transparent)]
    PortAllocatorError(#[from] PortAllocatorError),

    #[error(transparent)]
    IoError(#[from] std::io::Error),

    #[error(transparent)]
    FatFilesystemError(#[from] fat::Error),

    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

#[derive(Copy, Clone, Debug)]
pub enum BlockSize {
    Bytes512,
    Bytes4096,
}

impl BlockSize {
    fn bytes(&self) -> u64 {
        match self {
            BlockSize::Bytes512 => 512,
            BlockSize::Bytes4096 => 4096,
        }
    }
}

/// A trait for functions exposed by all disk backends (files, Crucible, etc.).
pub trait DiskConfig: std::fmt::Debug + Send + Sync {
    /// Yields the backend spec for this disk's storage backend.
    fn backend_spec(&self) -> (String, StorageBackendV0);

    /// Yields the guest OS kind of the guest image the disk was created from,
    /// or `None` if the disk was not created from a guest image.
    fn guest_os(&self) -> Option<GuestOsKind>;

    /// If this disk is a Crucible disk, yields `Some` reference to that disk as
    /// a Crucible disk.
    fn as_crucible(&self) -> Option<&CrucibleDisk> {
        None
    }
}

/// The possible sources for a disk's initial data.
#[derive(Clone, Debug)]
pub enum DiskSource<'a> {
    /// A blank disk with the supplied size, in bytes.
    Blank(usize),

    /// A disk backed by the guest image artifact with the supplied key.
    Artifact(&'a str),

    /// A disk with the contents of the supplied filesystem.
    FatFilesystem(fat::FatFilesystem),
}

impl DiskSource<'_> {
    pub(crate) fn kind(&self) -> &'static str {
        match self {
            DiskSource::Blank(_) => "blank",
            DiskSource::Artifact(_) => "artifact",
            DiskSource::FatFilesystem(_) => "filesystem",
        }
    }
}

/// A factory that provides tests with the means to create a disk they can
/// attach to a guest VM.
///
/// The `create_foo` functions implemented by the factory create disk objects
/// whose initial contents are described by a supplied [`DiskSource`]. They
/// return disks wrapped in an `Arc` that can be passed to `ConfigRequest`
/// routines that add disks to a VM's configuration. This allows tests to manage
/// disks in two ways:
///
/// 1. Tests that don't need a disk's resources to outlive a VM can simply move
///    the disk reference into the VM config (which will move the reference to
///    the VM). In this way the disk is destroyed when its test VM goes away.
/// 2. Tests that want to preserve or reuse a disk after its VM stops can
///    instead clone the reference into the VM and reuse the source reference
///    later in the test. This can be used to, say, launch a VM, destroy it, and
///    attach the same disk to another VM to verify that changes to it are
///    persisted.
///
/// N.B. The disk objects the factory creates take no special care to ensure
///      that they can be used safely by multiple VMs at the same time. If
///      multiple VMs do use a single set of backend resources, the resulting
///      behavior will depend on the chosen backend's semantics and the way the
///      Propolis backend implementations interact with the disk.
pub(crate) struct DiskFactory {
    /// The directory in which disk files should be stored.
    storage_dir: Utf8PathBuf,

    /// A reference to the artifact store to use to look up guest OS artifacts
    /// when those are used as a disk source.
    artifact_store: Arc<ArtifactStore>,

    /// The port allocator to use to allocate ports to Crucible server
    /// processes.
    port_allocator: Arc<PortAllocator>,

    /// The logging discipline to use for Crucible server processes.
    log_mode: ServerLogMode,
}

impl DiskFactory {
    /// Creates a new disk factory. The disks this factory generates will store
    /// their data in `storage_dir` and will look up guest OS images in the
    /// supplied `artifact_store`.
    pub fn new(
        storage_dir: &impl AsRef<Utf8Path>,
        artifact_store: Arc<ArtifactStore>,
        port_allocator: Arc<PortAllocator>,
        log_mode: ServerLogMode,
    ) -> Self {
        Self {
            storage_dir: storage_dir.as_ref().to_path_buf(),
            artifact_store,
            port_allocator,
            log_mode,
        }
    }
}

impl DiskFactory {
    async fn get_guest_artifact_info(
        &self,
        artifact_name: &str,
    ) -> Result<(Utf8PathBuf, GuestOsKind), DiskError> {
        self.artifact_store
            .get_guest_os_image(artifact_name)
            .await
            .with_context(|| {
                format!("failed to get guest OS artifact '{}'", artifact_name)
            })
            .map_err(Into::into)
    }

    /// Creates a new disk backed by a file whose initial contents are specified
    /// by `source`.
    pub(crate) async fn create_file_backed_disk<'d>(
        &self,
        name: String,
        source: &DiskSource<'d>,
    ) -> Result<Arc<FileBackedDisk>, DiskError> {
        let artifact_name = match source {
            DiskSource::Artifact(name) => name,
            // It's possible in theory to have a file-backed disk that isn't
            // backed by an artifact by creating a temporary file and copying
            // the supplied disk contents to it, but for now this isn't
            // supported.
            DiskSource::Blank(_) | DiskSource::FatFilesystem(_) => {
                return Err(DiskError::SourceNotSupported {
                    disk_type: "file-backed",
                    src: source.kind(),
                });
            }
        };

        let (artifact_path, guest_os) =
            self.get_guest_artifact_info(artifact_name).await?;

        FileBackedDisk::new_from_artifact(
            name,
            &artifact_path,
            &self.storage_dir,
            Some(guest_os),
        )
        .map(Arc::new)
    }

    /// Creates a new Crucible-backed disk by creating three region files to
    /// hold the disk's data and launching a Crucible downstairs process to
    /// serve each one.
    ///
    /// # Parameters
    ///
    /// - source: The data source that supplies the disk's initial contents.
    ///   If the source data is stored as a file on the local disk, the
    ///   resulting disk's `VolumeConstructionRequest`s will specify that this
    ///   file should be used as a read-only parent volume.
    /// - min_disk_size_gib: The disk's minimum size in GiB. The disk's actual
    ///   size is the larger of this size and the source's size.
    /// - block_size: The disk's block size.
    pub(crate) async fn create_crucible_disk<'d>(
        &self,
        name: String,
        source: &DiskSource<'d>,
        mut min_disk_size_gib: u64,
        block_size: BlockSize,
    ) -> Result<Arc<CrucibleDisk>, DiskError> {
        let binary_path = self.artifact_store.get_crucible_downstairs().await?;

        let (artifact_path, guest_os) = match source {
            DiskSource::Artifact(name) => {
                let (path, os) = self.get_guest_artifact_info(name).await?;
                (Some(path), Some(os))
            }
            DiskSource::Blank(size) => {
                min_disk_size_gib = min_disk_size_gib
                    .max(u64::try_from(*size).map_err(anyhow::Error::from)?);
                (None, None)
            }
            // It's possible in theory to have a Crucible-backed disk with
            // caller-supplied initial contents by writing those contents out to
            // intermediate files and using them as a read-only parent (or just
            // importing them directly into the Crucible regions), but for now
            // this isn't supported.
            DiskSource::FatFilesystem(_) => {
                return Err(DiskError::SourceNotSupported {
                    disk_type: "Crucible-backed",
                    src: source.kind(),
                });
            }
        };

        let mut ports = [0u16; 3];
        for port in &mut ports {
            *port = self.port_allocator.next()?;
        }

        CrucibleDisk::new(
            name,
            min_disk_size_gib,
            block_size,
            &binary_path.as_std_path(),
            &ports,
            &self.storage_dir,
            artifact_path.as_ref(),
            guest_os,
            self.log_mode,
        )
        .map(Arc::new)
        .map_err(Into::into)
    }

    pub(crate) async fn create_in_memory_disk<'d>(
        &self,
        name: String,
        source: &DiskSource<'d>,
        readonly: bool,
    ) -> Result<Arc<InMemoryDisk>, DiskError> {
        let contents = match source {
            DiskSource::Artifact(name) => {
                let (path, _) = self.get_guest_artifact_info(name).await?;
                std::fs::read(&path).with_context(|| {
                    format!("reading source artifact {} from {}", name, path)
                })?
            }
            DiskSource::Blank(size) => vec![0; *size],
            DiskSource::FatFilesystem(fs) => fs.as_bytes()?,
        };

        Ok(Arc::new(InMemoryDisk::new(name, contents, readonly)))
    }
}
