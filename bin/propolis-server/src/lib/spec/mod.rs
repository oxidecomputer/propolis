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

use crate::spec::api_spec_v6::ApiSpecError;
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
            VirtioSocket as VirtioSocketDesc,
        },
    },
    PciPath, SpecKey,
};
use propolis_api_types::instance_spec::{
    Component, InstanceSpec, SmbiosType1Input,
};
use propolis_api_types_versions::{v1, v3, v6, latest};
use thiserror::Error;

#[cfg(feature = "failure-injection")]
use propolis_api_types::instance_spec::components::devices::MigrationFailureInjector;

use propolis::firmware::acpi::AcpiVariant;
#[cfg(feature = "falcon")]
use propolis_api_types::instance_spec::components::{
    backends::DlpiNetworkBackend,
    devices::{P9fs, SoftNpuP9, SoftNpuPciPort},
};

// mod api_request;
pub(crate) mod api_spec_v1;
pub(crate) mod api_spec_v3;
pub(crate) mod api_spec_v6;
pub(crate) mod builder;

/*
/// TODO: it happens to be true that we can write this today. it is not true in general that `Spec`
/// must be convertible into `latest::instance_spec::InstanceSpec` (we can deprecate or remove an
/// item in a new InstanceSpec, and the conversion can become impossible. this just hasn't happened
/// yet.)
impl From<Spec> for latest::instance_spec::InstanceSpec {
    fn from(val: Spec) -> Self {
        api_spec_v6::
        // TODO:
        panic!("convert the spec into a latest::InstanceSpec or die trying");
    }
}
*/

/// The code related to latest types does not go into a versioned module
///
/// TODO: impls here can probably be copied wholesale when new versions are added
/// it may not be correct for a new `TryFrom<InstanceSpec> for Spec` to be derived from this
/// function. or it may be reasonable. this depends exclusively on the changes in InstanceSpec.
impl TryFrom<InstanceSpec> for Spec {
    type Error = ApiSpecError;

    fn try_from(value: InstanceSpec) -> Result<Self, Self::Error> {
        Ok(api_spec_v6::latest_api_spec_to_spec_builder(value)?.finish())
            /*
        // Extract vsock before conversion since it's v3-only and will be
        // filtered out during the v3→v2→v1 chain.
        let mut vsock_entry = None;
        for (id, component) in &value.components {
            if let Component::VirtioSocket(v) = component {
                vsock_entry = Some(VirtioSocket { id: id.clone(), spec: *v });
                break;
            }
        }

        let v3_spec: v3::instance_spec::InstanceSpec = value.into();
        let v2_spec: v2::instance_spec::InstanceSpec = v3_spec.into();
        let smbios = v2_spec.smbios.clone();
        let v1_spec: v1::instance_spec::InstanceSpec = v2_spec.into();

        let mut builder = api_spec_v1::v1_to_spec_builder(v1_spec)?;
        if let Some(vsock) = vsock_entry {
            builder.add_vsock_device(vsock)?;
        }
        let mut spec = builder.finish();
        spec.smbios_type1_input = smbios;
        Ok(spec)
            */
    }
}

// TODO: now.. what about the older versions of InstanceSpec!
impl TryFrom<v1::instance_spec::InstanceSpec> for Spec {
    type Error = api_spec_v1::ApiSpecError;

    fn try_from(value: v1::instance_spec::InstanceSpec) -> Result<Self, Self::Error> {
        Ok(api_spec_v1::v1_to_spec_builder(value)?.finish())
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

    pub vsock: Option<VirtioSocket>,

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
    // construct `Spec` from a function that takes an `v1::instance_spec::InstanceSpec` and the
    // VM UUID. This would replace `impl TryFrom<v1::instance_spec::InstanceSpec> for Spec`, and
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

    // XXX: expose via the API once more variants are implemented.
    pub acpi_variant: AcpiVariant,
}

impl Default for Board {
    fn default() -> Self {
        Self {
            cpus: 0,
            memory_mb: 0,
            chipset: Chipset::I440Fx(I440Fx { enable_pcie: false }),
            guest_hv_interface: GuestHypervisorInterface::Bhyve,
            acpi_variant: AcpiVariant::V0,
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

impl From<StorageDevice> for latest::instance_spec::Component {
    fn from(value: StorageDevice) -> Self {
        match value {
            StorageDevice::Virtio(d) => Self::VirtioDisk(d),
            StorageDevice::Nvme(d) => Self::NvmeDisk(d),
        }
    }
}

// TODO: this needs to move or die
impl TryFrom<StorageDevice> for v3::instance_spec::Component {
    type Error = v6::instance_spec::InvalidV3Component;

    fn try_from(value: StorageDevice) -> Result<Self, Self::Error> {
        match value {
            StorageDevice::Virtio(d) => Ok(Self::VirtioDisk(d)),
            StorageDevice::Nvme(d) => Ok(Self::NvmeDisk(d.try_into()?)),
        }
    }
}

// TODO: this needs to move or die
impl TryFrom<StorageDevice> for v1::instance_spec::Component {
    type Error = v6::instance_spec::InvalidV3Component;

    fn try_from(value: StorageDevice) -> Result<Self, Self::Error> {
        match value {
            StorageDevice::Virtio(d) => Ok(Self::VirtioDisk(d)),
            StorageDevice::Nvme(d) => Ok(Self::NvmeDisk(d.try_into()?)),
        }
    }
}

impl TryFrom<Component> for StorageDevice {
    type Error = ComponentTypeMismatch;

    fn try_from(
        value: Component,
    ) -> Result<Self, Self::Error> {
        match value {
            Component::VirtioDisk(d) => Ok(Self::Virtio(d)),
            Component::NvmeDisk(d) => Ok(Self::Nvme(d)),
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

impl From<StorageBackend> for Component {
    fn from(value: StorageBackend) -> Self {
        match value {
            StorageBackend::Crucible(be) => Self::CrucibleStorageBackend(be),
            StorageBackend::File(be) => Self::FileStorageBackend(be),
            StorageBackend::Blob(be) => Self::BlobStorageBackend(be),
        }
    }
}

impl From<StorageBackend> for v3::instance_spec::Component {
    fn from(value: StorageBackend) -> Self {
        match value {
            StorageBackend::Crucible(be) => Self::CrucibleStorageBackend(be),
            StorageBackend::File(be) => Self::FileStorageBackend(be),
            StorageBackend::Blob(be) => Self::BlobStorageBackend(be),
        }
    }
}

impl From<StorageBackend> for v1::instance_spec::Component {
    fn from(value: StorageBackend) -> Self {
        match value {
            StorageBackend::Crucible(be) => Self::CrucibleStorageBackend(be),
            StorageBackend::File(be) => Self::FileStorageBackend(be),
            StorageBackend::Blob(be) => Self::BlobStorageBackend(be),
        }
    }
}

impl TryFrom<Component> for StorageBackend {
    type Error = ComponentTypeMismatch;

    fn try_from(
        value: Component,
    ) -> Result<Self, Self::Error> {
        match value {
            Component::CrucibleStorageBackend(be) => {
                Ok(Self::Crucible(be))
            }
            Component::FileStorageBackend(be) => {
                Ok(Self::File(be))
            }
            Component::BlobStorageBackend(be) => {
                Ok(Self::Blob(be))
            }
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

#[derive(Clone, Debug)]
pub struct VirtioSocket {
    pub id: SpecKey,
    pub spec: VirtioSocketDesc,
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
