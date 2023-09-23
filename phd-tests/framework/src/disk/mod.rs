// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Routines for creating and managing guest disks.
//!
//! Test cases create disks using the [`DiskFactory`] in their test contexts.
//! They can then pass these disks to the VM factory to connect them to a
//! specific guest VM.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::Context;
use propolis_client::instance_spec::v0::StorageBackendV0;
use thiserror::Error;

use crate::{
    artifacts::ArtifactStore,
    guest_os::GuestOsKind,
    port_allocator::{PortAllocator, PortAllocatorError},
    server_log_mode::ServerLogMode,
};

use self::{crucible::CrucibleDisk, file::FileBackedDisk};

pub mod crucible;
mod file;

/// Errors that can arise while working with disks.
#[derive(Debug, Error)]
pub enum DiskError {
    #[error("Disk factory has no Crucible downstairs path")]
    NoCrucibleDownstairsPath,

    #[error(transparent)]
    PortAllocatorError(#[from] PortAllocatorError),

    #[error(transparent)]
    IoError(#[from] std::io::Error),

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
pub trait DiskConfig: std::fmt::Debug {
    /// Yields the backend spec for this disk's storage backend.
    fn backend_spec(&self) -> StorageBackendV0;

    /// Yields the guest OS kind of the guest image the disk was created from,
    /// or `None` if the disk was not created from a guest image.
    fn guest_os(&self) -> Option<GuestOsKind>;
}

/// The possible sources for a disk's initial data.
#[derive(Debug)]
pub enum DiskSource<'a> {
    /// A disk backed by the guest image artifact with the supplied key.
    Artifact(&'a str),
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
pub struct DiskFactory<'a> {
    /// The directory in which disk files should be stored.
    storage_dir: PathBuf,

    /// A reference to the artifact store to use to look up guest OS artifacts
    /// when those are used as a disk source.
    artifact_store: &'a ArtifactStore,

    /// The path to the Crucible downstairs binary to launch to serve Crucible
    /// disks.
    crucible_downstairs_binary: Option<PathBuf>,

    /// The port allocator to use to allocate ports to Crucible server
    /// processes.
    port_allocator: &'a PortAllocator,

    /// The logging discipline to use for Crucible server processes.
    log_mode: ServerLogMode,
}

impl<'a> DiskFactory<'a> {
    /// Creates a new disk factory. The disks this factory generates will store
    /// their data in `storage_dir` and will look up guest OS images in the
    /// supplied `artifact_store`.
    pub fn new(
        storage_dir: &impl AsRef<Path>,
        artifact_store: &'a ArtifactStore,
        crucible_downstairs_binary: Option<&impl AsRef<Path>>,
        port_allocator: &'a PortAllocator,
        log_mode: ServerLogMode,
    ) -> Self {
        Self {
            storage_dir: storage_dir.as_ref().to_path_buf(),
            artifact_store,
            crucible_downstairs_binary: crucible_downstairs_binary
                .map(|p| p.as_ref().to_path_buf()),
            port_allocator,
            log_mode,
        }
    }
}

impl DiskFactory<'_> {
    fn get_guest_artifact_info(
        &self,
        artifact_name: &str,
    ) -> Result<(PathBuf, GuestOsKind), DiskError> {
        self.artifact_store
            .get_guest_os_image(artifact_name)
            .map(|(utf8, kind)| (utf8.into_std_path_buf(), kind))
            .with_context(|| {
                format!("failed to get guest OS artifact '{}'", artifact_name)
            })
            .map_err(Into::into)
    }

    /// Creates a new disk backed by a file whose initial contents are specified
    /// by `source`.
    pub fn create_file_backed_disk(
        &self,
        source: DiskSource,
    ) -> Result<Arc<FileBackedDisk>, DiskError> {
        let DiskSource::Artifact(artifact_name) = source;
        let (artifact_path, guest_os) =
            self.get_guest_artifact_info(artifact_name)?;

        FileBackedDisk::new_from_artifact(
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
    /// - disk_size_gib: The disk's expected size in GiB.
    /// - block_size: The disk's block size.
    pub fn create_crucible_disk(
        &self,
        source: DiskSource,
        disk_size_gib: u64,
        block_size: BlockSize,
    ) -> Result<Arc<CrucibleDisk>, DiskError> {
        let binary_path = self
            .crucible_downstairs_binary
            .as_ref()
            .ok_or(DiskError::NoCrucibleDownstairsPath)?;

        let DiskSource::Artifact(artifact_name) = source;
        let (artifact_path, guest_os) =
            self.get_guest_artifact_info(artifact_name)?;

        let mut ports = [0u16; 3];
        for port in &mut ports {
            *port = self.port_allocator.next()?;
        }

        CrucibleDisk::new(
            disk_size_gib,
            block_size,
            binary_path,
            &ports,
            &self.storage_dir,
            Some(&artifact_path),
            Some(guest_os),
            self.log_mode,
        )
        .map(Arc::new)
        .map_err(Into::into)
    }
}
