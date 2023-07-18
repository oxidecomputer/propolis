//! Version 0 of a fully-composed instance specification.
//!
//! V0 specs are split into 'device' and 'backend' halves that can be serialized
//! and deserialized independently.
//!
//! # Versioning and compatibility
//!
//! Changes to structs and enums in this module must be backward-compatible
//! (i.e. new code must be able to deserialize specs created by old versions of
//! the module). Breaking changes to the spec structure must be turned into a
//! new specification version. Note that adding a new component to one of the
//! existing enums in this module is not a back-compat breaking change.
//!
//! Data types in this module should have a `V0` suffix in their names to avoid
//! aliasing with type names in other versions (which can cause Dropshot to
//! create OpenAPI specs that are missing certain types; see dropshot#383).

use std::collections::HashMap;

use crate::instance_spec::{components, PciPath, SpecKey};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub mod builder;

#[derive(Clone, Deserialize, Serialize, Debug, JsonSchema)]
#[serde(deny_unknown_fields, tag = "type", content = "component")]
pub enum StorageDeviceV0 {
    VirtioDisk(components::devices::VirtioDisk),
    NvmeDisk(components::devices::NvmeDisk),
}

impl StorageDeviceV0 {
    fn pci_path(&self) -> PciPath {
        match self {
            Self::VirtioDisk(disk) => disk.pci_path,
            Self::NvmeDisk(disk) => disk.pci_path,
        }
    }
}

#[derive(Clone, Deserialize, Serialize, Debug, JsonSchema)]
#[serde(deny_unknown_fields, tag = "type", content = "component")]
pub enum NetworkDeviceV0 {
    VirtioNic(components::devices::VirtioNic),
}

impl NetworkDeviceV0 {
    fn pci_path(&self) -> PciPath {
        match self {
            Self::VirtioNic(nic) => nic.pci_path,
        }
    }
}

#[derive(Default, Clone, Deserialize, Serialize, Debug, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DeviceSpecV0 {
    pub board: components::board::Board,
    pub storage_devices: HashMap<SpecKey, StorageDeviceV0>,
    pub network_devices: HashMap<SpecKey, NetworkDeviceV0>,
    pub serial_ports: HashMap<SpecKey, components::devices::SerialPort>,
    pub pci_pci_bridges: HashMap<SpecKey, components::devices::PciPciBridge>,

    #[cfg(feature = "falcon")]
    pub softnpu_pci_port: Option<components::devices::SoftNpuPciPort>,
    #[cfg(feature = "falcon")]
    pub softnpu_ports: HashMap<SpecKey, components::devices::SoftNpuPort>,
    #[cfg(feature = "falcon")]
    pub softnpu_p9: Option<components::devices::SoftNpuP9>,
    #[cfg(feature = "falcon")]
    pub p9fs: Option<components::devices::P9fs>,
}

#[derive(Clone, Deserialize, Serialize, Debug, JsonSchema)]
#[serde(deny_unknown_fields, tag = "type", content = "component")]
pub enum StorageBackendV0 {
    Crucible(components::backends::CrucibleStorageBackend),
    File(components::backends::FileStorageBackend),
    InMemory(components::backends::InMemoryStorageBackend),
}

#[derive(Clone, Deserialize, Serialize, Debug, JsonSchema)]
#[serde(deny_unknown_fields, tag = "type", content = "component")]
pub enum NetworkBackendV0 {
    Virtio(components::backends::VirtioNetworkBackend),
    Dlpi(components::backends::DlpiNetworkBackend),
}

#[derive(Default, Clone, Deserialize, Serialize, Debug, JsonSchema)]
pub struct BackendSpecV0 {
    pub storage_backends: HashMap<SpecKey, StorageBackendV0>,
    pub network_backends: HashMap<SpecKey, NetworkBackendV0>,
}

#[derive(Default, Clone, Deserialize, Serialize, Debug, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct InstanceSpecV0 {
    pub devices: DeviceSpecV0,
    pub backends: BackendSpecV0,
}
