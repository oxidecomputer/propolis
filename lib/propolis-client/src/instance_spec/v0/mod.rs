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

use crate::instance_spec::{
    components,
    migration::{MigrationCollection, MigrationElement},
    PciPath, SpecKey,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::migration::{
    ElementCompatibilityError, MigrationCompatibilityError,
};

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

impl MigrationElement for StorageDeviceV0 {
    fn kind(&self) -> &'static str {
        match self {
            StorageDeviceV0::VirtioDisk(_) => "StorageDevice(VirtioDisk)",
            StorageDeviceV0::NvmeDisk(_) => "StorageDevice(NvmeDisk)",
        }
    }

    fn can_migrate_from_element(
        &self,
        other: &Self,
    ) -> Result<(), super::migration::ElementCompatibilityError> {
        match (self, other) {
            (Self::VirtioDisk(this), Self::VirtioDisk(other)) => {
                this.can_migrate_from_element(other)
            }
            (Self::NvmeDisk(this), Self::NvmeDisk(other)) => {
                this.can_migrate_from_element(other)
            }
            (_, _) => Err(ElementCompatibilityError::ComponentsIncomparable(
                self.kind(),
                other.kind(),
            )),
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

impl MigrationElement for NetworkDeviceV0 {
    fn kind(&self) -> &'static str {
        "NetworkDevice(VirtioNic)"
    }

    fn can_migrate_from_element(
        &self,
        other: &Self,
    ) -> Result<(), ElementCompatibilityError> {
        let (Self::VirtioNic(this), Self::VirtioNic(other)) = (self, other);
        this.can_migrate_from_element(other)
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

impl DeviceSpecV0 {
    pub fn can_migrate_devices_from(
        &self,
        other: &Self,
    ) -> Result<(), MigrationCompatibilityError> {
        self.board.can_migrate_from_element(&other.board).map_err(|e| {
            MigrationCompatibilityError::ElementMismatch(
                "board".to_string(),
                e.into(),
            )
        })?;

        self.storage_devices
            .can_migrate_from_collection(&other.storage_devices)
            .map_err(|e| {
                MigrationCompatibilityError::CollectionMismatch(
                    "storage devices".to_string(),
                    e,
                )
            })?;

        self.network_devices
            .can_migrate_from_collection(&other.network_devices)
            .map_err(|e| {
                MigrationCompatibilityError::CollectionMismatch(
                    "storage devices".to_string(),
                    e,
                )
            })?;

        self.serial_ports
            .can_migrate_from_collection(&other.serial_ports)
            .map_err(|e| {
                MigrationCompatibilityError::CollectionMismatch(
                    "serial ports".to_string(),
                    e,
                )
            })?;

        self.pci_pci_bridges
            .can_migrate_from_collection(&other.pci_pci_bridges)
            .map_err(|e| {
                MigrationCompatibilityError::CollectionMismatch(
                    "PCI bridges".to_string(),
                    e,
                )
            })?;

        Ok(())
    }
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
