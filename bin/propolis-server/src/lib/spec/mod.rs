// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Instance specs describe how to configure a VM and what components it has.
//!
//! This module defines a crate-internal instance spec type, [`Spec`], and its
//! constituent types, like [`Disk`] and [`Nic`]. Unlike the types in
//! [`propolis_api_types::instance_spec`], these internal types are not
//! `Serialize` and are never meant to be used over the wire in API requests or
//! the migration protocol. This allows them to change freely between Propolis
//! versions, so long as they can consistently be converted to and from the
//! wire-format types in the [`propolis_api_types`] crate. This, in turn, allows
//! [`Spec`] and its component types to take forms that might otherwise be hard
//! to change in a backward-compatible way.

use std::collections::HashMap;

use propolis::cpuid::Set as CpuidSet;
use propolis_api_types::instance_spec::{
    components::{
        backends::{
            BlobStorageBackend, CrucibleStorageBackend, FileStorageBackend,
            VirtioNetworkBackend,
        },
        board::{Chipset, I440Fx},
        devices::{
            NvmeDisk, PciPciBridge, QemuPvpanic as QemuPvpanicDesc,
            SerialPortNumber, VirtioDisk, VirtioNic,
        },
    },
    v0::ComponentV0,
    PciPath,
};
use thiserror::Error;

#[cfg(feature = "falcon")]
use propolis_api_types::instance_spec::components::{
    backends::DlpiNetworkBackend,
    devices::{P9fs, SoftNpuP9, SoftNpuPciPort},
};

mod api_request;
pub(crate) mod api_spec_v0;
pub(crate) mod builder;
mod config_toml;

#[derive(Debug, Error)]
#[error("input component type can't convert to output type")]
pub struct ComponentTypeMismatch;

/// An instance specification that describes a VM's configuration and
/// components.
///
/// NOTE: This struct's fields are `pub` to make it convenient to access the
/// individual parts of a fully-constructed spec. Modules that consume specs may
/// assert that they are valid (no duplicate component names, no duplicate PCI
/// device paths, etc.). When constructing a new spec, use the
/// [`builder::SpecBuilder`] struct to catch requests that violate these
/// invariants.
#[derive(Clone, Debug, Default)]
pub(crate) struct Spec {
    pub board: Board,
    pub cpuid: Option<CpuidSet>,
    pub disks: HashMap<String, Disk>,
    pub nics: HashMap<String, Nic>,
    pub boot_settings: Option<BootSettings>,

    pub serial: HashMap<String, SerialPort>,

    pub pci_pci_bridges: HashMap<String, PciPciBridge>,
    pub pvpanic: Option<QemuPvpanic>,

    #[cfg(feature = "falcon")]
    pub softnpu: SoftNpu,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct Board {
    pub cpus: u8,
    pub memory_mb: u64,
    pub chipset: Chipset,
}

impl Default for Board {
    fn default() -> Self {
        Self {
            cpus: 0,
            memory_mb: 0,
            chipset: Chipset::I440Fx(I440Fx { enable_pcie: false }),
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct BootSettings {
    pub name: String,
    pub order: Vec<BootOrderEntry>,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct BootOrderEntry {
    pub name: String,
}

impl
    From<propolis_api_types::instance_spec::components::devices::BootOrderEntry>
    for BootOrderEntry
{
    fn from(
        value: propolis_api_types::instance_spec::components::devices::BootOrderEntry,
    ) -> Self {
        Self { name: value.name.clone() }
    }
}

impl From<BootOrderEntry>
    for propolis_api_types::instance_spec::components::devices::BootOrderEntry
{
    fn from(value: BootOrderEntry) -> Self {
        Self { name: value.name }
    }
}

/// Describes the device half of a [`Disk`].
#[derive(Clone, Debug)]
pub enum StorageDevice {
    Virtio(VirtioDisk),
    Nvme(NvmeDisk),
}

impl StorageDevice {
    pub fn kind(&self) -> &'static str {
        match self {
            StorageDevice::Virtio(_) => "virtio",
            StorageDevice::Nvme(_) => "nvme",
        }
    }

    pub fn pci_path(&self) -> PciPath {
        match self {
            StorageDevice::Virtio(disk) => disk.pci_path,
            StorageDevice::Nvme(disk) => disk.pci_path,
        }
    }

    pub fn backend_name(&self) -> &str {
        match self {
            StorageDevice::Virtio(disk) => &disk.backend_name,
            StorageDevice::Nvme(disk) => &disk.backend_name,
        }
    }
}

impl From<StorageDevice> for ComponentV0 {
    fn from(value: StorageDevice) -> Self {
        match value {
            StorageDevice::Virtio(d) => Self::VirtioDisk(d),
            StorageDevice::Nvme(d) => Self::NvmeDisk(d),
        }
    }
}

impl TryFrom<ComponentV0> for StorageDevice {
    type Error = ComponentTypeMismatch;

    fn try_from(value: ComponentV0) -> Result<Self, Self::Error> {
        match value {
            ComponentV0::VirtioDisk(d) => Ok(Self::Virtio(d)),
            ComponentV0::NvmeDisk(d) => Ok(Self::Nvme(d)),
            _ => Err(ComponentTypeMismatch),
        }
    }
}

/// Describes the backend half of a [`Disk`].
#[derive(Clone, Debug)]
pub enum StorageBackend {
    Crucible(CrucibleStorageBackend),
    File(FileStorageBackend),
    Blob(BlobStorageBackend),
}

impl StorageBackend {
    pub fn kind(&self) -> &'static str {
        match self {
            StorageBackend::Crucible(_) => "crucible",
            StorageBackend::File(_) => "file",
            StorageBackend::Blob(_) => "backend",
        }
    }

    pub fn read_only(&self) -> bool {
        match self {
            StorageBackend::Crucible(be) => be.readonly,
            StorageBackend::File(be) => be.readonly,
            StorageBackend::Blob(be) => be.readonly,
        }
    }
}

impl From<StorageBackend> for ComponentV0 {
    fn from(value: StorageBackend) -> Self {
        match value {
            StorageBackend::Crucible(be) => Self::CrucibleStorageBackend(be),
            StorageBackend::File(be) => Self::FileStorageBackend(be),
            StorageBackend::Blob(be) => Self::BlobStorageBackend(be),
        }
    }
}

impl TryFrom<ComponentV0> for StorageBackend {
    type Error = ComponentTypeMismatch;

    fn try_from(value: ComponentV0) -> Result<Self, Self::Error> {
        match value {
            ComponentV0::CrucibleStorageBackend(be) => Ok(Self::Crucible(be)),
            ComponentV0::FileStorageBackend(be) => Ok(Self::File(be)),
            ComponentV0::BlobStorageBackend(be) => Ok(Self::Blob(be)),
            _ => Err(ComponentTypeMismatch),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Disk {
    pub device_spec: StorageDevice,
    pub backend_spec: StorageBackend,
}

#[derive(Clone, Debug)]
pub struct Nic {
    pub device_spec: VirtioNic,
    pub backend_spec: VirtioNetworkBackend,
}

/// A kind of device to install as the listener on a COM port.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SerialPortDevice {
    Uart,

    #[cfg(feature = "falcon")]
    SoftNpu,
}

impl std::fmt::Display for SerialPortDevice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}",
            match self {
                SerialPortDevice::Uart => "uart",

                #[cfg(feature = "falcon")]
                SerialPortDevice::SoftNpu => "softnpu",
            }
        )
    }
}

#[derive(Clone, Debug)]
pub struct SerialPort {
    pub num: SerialPortNumber,
    pub device: SerialPortDevice,
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

#[cfg(feature = "falcon")]
impl SoftNpu {
    /// Returns `true` if this struct specifies at least one SoftNPU component.
    pub fn has_components(&self) -> bool {
        self.pci_port.is_some()
            || self.p9_device.is_some()
            || self.p9fs.is_some()
            || !self.ports.is_empty()
    }
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
