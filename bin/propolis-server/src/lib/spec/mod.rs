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

use std::collections::BTreeMap;

use cpuid_utils::CpuidSet;
use propolis_api_types::instance_spec::{
    components::{
        backends::{
            BlobStorageBackend, CrucibleStorageBackend, FileStorageBackend,
            VirtioNetworkBackend,
        },
        board::{Chipset, GuestHypervisorInterface, I440Fx},
        devices::{
            NvmeDisk, PciPciBridge, QemuPvpanic as QemuPvpanicDesc,
            SerialPortNumber, VirtioDisk, VirtioNic,
        },
    },
    v0::ComponentV0,
    InstanceSpec, PciPath, SmbiosType1Input, SpecKey,
};
use thiserror::Error;

#[cfg(feature = "failure-injection")]
use propolis_api_types::instance_spec::components::devices::MigrationFailureInjector;

#[cfg(feature = "falcon")]
use propolis_api_types::instance_spec::components::{
    backends::DlpiNetworkBackend,
    devices::{P9fs, SoftNpuP9, SoftNpuPciPort},
};

// mod api_request;
pub(crate) mod api_spec_v0;
pub(crate) mod builder;

#[derive(Debug, Error)]
#[error("missing smbios_type1_input")]
pub(crate) struct MissingSmbiosError;

/// The code related to latest types does not go into a versioned module
impl TryFrom<Spec> for InstanceSpec {
    type Error = MissingSmbiosError;

    fn try_from(val: Spec) -> Result<Self, Self::Error> {
        let Some(smbios) = val.smbios_type1_input.clone() else {
            return Err(MissingSmbiosError);
        };

        let InstanceSpecV0 { board, components } = InstanceSpecV0::from(val);
        Ok(InstanceSpec { board, components, smbios })
    }
}

/// The code related to latest types does not go into a versioned module
impl TryFrom<InstanceSpec> for Spec {
    type Error = ApiSpecError;

    fn try_from(value: InstanceSpec) -> Result<Self, Self::Error> {
        let InstanceSpecV1 { board, components, smbios } = value;
        let v0 = InstanceSpecV0 { board, components };
        let mut spec: Spec = v0.try_into()?;
        spec.smbios_type1_input = Some(smbios);
        Ok(spec)
    }
}

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
    pub cpuid: CpuidSet,
    pub disks: BTreeMap<SpecKey, Disk>,
    pub nics: BTreeMap<SpecKey, Nic>,
    pub boot_settings: Option<BootSettings>,

    pub serial: BTreeMap<SpecKey, SerialPort>,

    pub pci_pci_bridges: BTreeMap<SpecKey, PciPciBridge>,
    pub pvpanic: Option<QemuPvpanic>,

    #[cfg(feature = "failure-injection")]
    pub migration_failure: Option<MigrationFailure>,

    #[cfg(feature = "falcon")]
    pub softnpu: SoftNpu,

    // TODO: This is an option because there is no good way to generate a
    // default implementation of `SmbiosType1Input`. The default `serial_number`
    // field of `SmbiosType1Input` should be equivalent to the VM UUID for
    // backwards compatibility, but that isn't currently possible.
    //
    // One way to fix this would be to remove the `Builder` and directly
    // construct `Spec` from a function that takes an `InstanceSpecV0` and the
    // VM UUID. This would replace `impl TryFrom<InstanceSpecV0> for Spec`, and
    // would allow removing the `Default` derive on `Spec`, and the `Option`
    // from the `smbios_type1_input` field.
    pub smbios_type1_input: Option<SmbiosType1Input>,
}

/// The VM's mainboard.
///
/// This is distinct from the [instance spec `Board`] so that it can exclude
/// fields (such as CPUID information) that need to be checked for validity
/// before being included in an internal spec.
///
/// [instance spec `Board`]: propolis_api_types::instance_spec::components::board::Board
#[derive(Clone, Debug)]
pub(crate) struct Board {
    pub cpus: u8,
    pub memory_mb: u64,
    pub chipset: Chipset,
    pub guest_hv_interface: GuestHypervisorInterface,
}

impl Default for Board {
    fn default() -> Self {
        Self {
            cpus: 0,
            memory_mb: 0,
            chipset: Chipset::I440Fx(I440Fx { enable_pcie: false }),
            guest_hv_interface: GuestHypervisorInterface::Bhyve,
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct BootSettings {
    pub name: SpecKey,
    pub order: Vec<BootOrderEntry>,
}

#[derive(Clone, Debug)]
pub(crate) struct BootOrderEntry {
    pub device_id: SpecKey,
}

impl
    From<propolis_api_types::instance_spec::components::devices::BootOrderEntry>
    for BootOrderEntry
{
    fn from(
        value: propolis_api_types::instance_spec::components::devices::BootOrderEntry,
    ) -> Self {
        Self { device_id: value.id.clone() }
    }
}

impl From<BootOrderEntry>
    for propolis_api_types::instance_spec::components::devices::BootOrderEntry
{
    fn from(value: BootOrderEntry) -> Self {
        Self { id: value.device_id }
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

    pub fn backend_id(&self) -> &SpecKey {
        match self {
            StorageDevice::Virtio(disk) => &disk.backend_id,
            StorageDevice::Nvme(disk) => &disk.backend_id,
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
    pub id: SpecKey,
    pub spec: QemuPvpanicDesc,
}

#[cfg(feature = "failure-injection")]
#[derive(Clone, Debug)]
pub struct MigrationFailure {
    pub id: SpecKey,
    pub spec: MigrationFailureInjector,
}

#[cfg(feature = "falcon")]
#[derive(Clone, Debug)]
pub struct SoftNpuPort {
    pub link_name: String,
    pub backend_name: SpecKey,
    pub backend_spec: DlpiNetworkBackend,
}

#[cfg(feature = "falcon")]
#[derive(Clone, Debug, Default)]
pub struct SoftNpu {
    pub pci_port: Option<SoftNpuPciPort>,
    pub ports: BTreeMap<SpecKey, SoftNpuPort>,
    pub p9_device: Option<SoftNpuP9>,
    pub p9fs: Option<P9fs>,
}
