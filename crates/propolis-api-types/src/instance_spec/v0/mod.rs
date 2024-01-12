// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

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
    migration::{
        ElementCompatibilityError, MigrationCollection,
        MigrationCompatibilityError, MigrationElement,
    },
    PciPath, SpecKey,
};
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

    // This field has a default value (`None`) to allow for
    // backwards-compatibility when upgrading from a Propolis
    // version that does not support this device. If the pvpanic device was not
    // present in the spec being deserialized, a `None` will be produced,
    // rather than rejecting the spec.
    #[serde(default)]
    // Skip serializing this field if it is `None`. This is so that Propolis
    // versions with support for this device are backwards-compatible with
    // older versions that don't, as long as the spec doesn't define a pvpanic
    // device --- if there is no panic device, skipping the field from the spec
    // means that the older version will still accept the spec.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub qemu_pvpanic: Option<components::devices::QemuPvpanic>,

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
            MigrationCompatibilityError::ElementMismatch("board".to_string(), e)
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

        self.qemu_pvpanic
            .can_migrate_from_element(&other.qemu_pvpanic)
            .map_err(|e| {
                MigrationCompatibilityError::ElementMismatch(
                    "QEMU PVPANIC device".to_string(),
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
    Blob(components::backends::BlobStorageBackend),
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
