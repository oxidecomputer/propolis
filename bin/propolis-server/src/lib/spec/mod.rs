// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Instance specs describe how to configure a VM and what components it has.
//!
//! This module defines an "internal" spec type for the server's instance
//! initialization code to use. This spec type is not `Serialize` and is not
//! meant to be sent over the wire in API requests or the migration protocol.
//! This allows the internal representation to change freely between versions
//! (so long as it can consistently be converted to and from the wire-format
//! spec in the [`propolis_api_types`] crate); this, in turn, allows this
//! representation to take forms that might otherwise be hard to change in a
//! backward-compatible way.

use std::collections::HashMap;

use propolis_api_types::instance_spec::{
    components::{
        backends::{
            BlobStorageBackend, CrucibleStorageBackend, FileStorageBackend,
            VirtioNetworkBackend,
        },
        board::Board,
        devices::{
            NvmeDisk, PciPciBridge, QemuPvpanic as QemuPvpanicDesc,
            SerialPortNumber, VirtioDisk, VirtioNic,
        },
    },
    v0::{StorageBackendV0, StorageDeviceV0},
    PciPath,
};

#[cfg(feature = "falcon")]
use propolis_api_types::instance_spec::components::{
    backends::DlpiNetworkBackend,
    devices::{P9fs, SoftNpuP9, SoftNpuPciPort},
};

mod api_request;
pub(crate) mod api_spec_v0;
pub(crate) mod builder;
mod config_toml;

#[derive(Clone, Debug, Default)]
pub(crate) struct Spec {
    pub board: Board,
    pub disks: HashMap<String, Disk>,
    pub nics: HashMap<String, Nic>,

    pub serial: HashMap<SerialPortNumber, SerialPortUser>,

    pub pci_pci_bridges: HashMap<String, PciPciBridge>,
    pub pvpanic: Option<QemuPvpanic>,

    #[cfg(feature = "falcon")]
    pub softnpu: SoftNpu,
}

/// Describes a storage device/backend pair parsed from an input source like an
/// API request or a config TOML entry.
#[derive(Clone, Debug)]
pub enum StorageDevice {
    Virtio(VirtioDisk),
    Nvme(NvmeDisk),
}

impl StorageDevice {
    pub fn pci_path(&self) -> PciPath {
        match self {
            StorageDevice::Virtio(disk) => disk.pci_path,
            StorageDevice::Nvme(disk) => disk.pci_path,
        }
    }
}

impl From<StorageDevice> for StorageDeviceV0 {
    fn from(value: StorageDevice) -> Self {
        match value {
            StorageDevice::Virtio(d) => Self::VirtioDisk(d),
            StorageDevice::Nvme(d) => Self::NvmeDisk(d),
        }
    }
}

impl From<StorageDeviceV0> for StorageDevice {
    fn from(value: StorageDeviceV0) -> Self {
        match value {
            StorageDeviceV0::VirtioDisk(d) => Self::Virtio(d),
            StorageDeviceV0::NvmeDisk(d) => Self::Nvme(d),
        }
    }
}

#[derive(Clone, Debug)]
pub enum StorageBackend {
    Crucible(CrucibleStorageBackend),
    File(FileStorageBackend),
    Blob(BlobStorageBackend),
}

impl From<StorageBackend> for StorageBackendV0 {
    fn from(value: StorageBackend) -> Self {
        match value {
            StorageBackend::Crucible(be) => Self::Crucible(be),
            StorageBackend::File(be) => Self::File(be),
            StorageBackend::Blob(be) => Self::Blob(be),
        }
    }
}

impl From<StorageBackendV0> for StorageBackend {
    fn from(value: StorageBackendV0) -> Self {
        match value {
            StorageBackendV0::Crucible(be) => Self::Crucible(be),
            StorageBackendV0::File(be) => Self::File(be),
            StorageBackendV0::Blob(be) => Self::Blob(be),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Disk {
    pub device_spec: StorageDevice,
    pub backend_name: String,
    pub backend_spec: StorageBackend,
}

#[derive(Clone, Debug)]
pub struct Nic {
    pub device_spec: VirtioNic,
    pub backend_name: String,
    pub backend_spec: VirtioNetworkBackend,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SerialPortUser {
    Standard,

    #[cfg(feature = "falcon")]
    SoftNpu,
}

#[derive(Clone, Debug)]
pub struct QemuPvpanic {
    #[allow(dead_code)]
    pub name: String,
    pub spec: QemuPvpanicDesc,
}

#[cfg(feature = "falcon")]
#[derive(Clone, Debug)]
pub struct SoftNpuPort {
    pub backend_name: String,
    pub backend_spec: DlpiNetworkBackend,
}

#[cfg(feature = "falcon")]
#[derive(Clone, Debug, Default)]
pub struct SoftNpu {
    pub pci_port: Option<SoftNpuPciPort>,
    pub ports: HashMap<String, SoftNpuPort>,
    pub p9_device: Option<SoftNpuP9>,
    pub p9fs: Option<P9fs>,
}

struct ParsedDiskRequest {
    name: String,
    disk: Disk,
}

struct ParsedNicRequest {
    name: String,
    nic: Nic,
}

struct ParsedPciBridgeRequest {
    name: String,
    bridge: PciPciBridge,
}

#[cfg(feature = "falcon")]
struct ParsedSoftNpuPort {
    name: String,
    port: SoftNpuPort,
}

#[cfg(feature = "falcon")]
#[derive(Default)]
struct ParsedSoftNpu {
    pub pci_port: Option<SoftNpuPciPort>,
    pub ports: Vec<ParsedSoftNpuPort>,
    pub p9_device: Option<SoftNpuP9>,
    pub p9fs: Option<P9fs>,
}

/// Generates NIC device and backend names from the NIC's PCI path. This is
/// needed because the `name` field in a propolis-client
/// `NetworkInterfaceRequest` is actually the name of the host vNIC to bind to,
/// and that can change between incarnations of an instance. The PCI path is
/// unique to each NIC but must remain stable over a migration, so it's suitable
/// for use in this naming scheme.
///
/// N.B. Migrating a NIC requires the source and target to agree on these names,
///      so changing this routine's behavior will prevent Propolis processes
///      with the old behavior from migrating processes with the new behavior.
fn pci_path_to_nic_names(path: PciPath) -> (String, String) {
    (format!("vnic-{}", path), format!("vnic-{}-backend", path))
}
