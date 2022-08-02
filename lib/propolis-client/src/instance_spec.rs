//! Instance specifications: abstract descriptions of a VM's devices and config.
//!
//! An instance spec describes a VM's virtual devices, backends, and other
//! guest environment configuration supplied by the Propolis VMM. RFD 283
//! contains more details about how specs are used throughout the Oxide stack.
//!
//! # Spec format
//!
//! Instance specs describe a VM's virtual mainboard (its CPUs, memory, chipset,
//! and platform details) and collections of virtual hardware components. Some
//! components, such as serial ports, are freestanding; others are split into a
//! "device" frontend, which specifies the virtual hardware interface exposed to
//! guest software, and a backend that provides services (e.g. durable storage
//! or a network interface) to devices from the host system and/or the rest of
//! the rack.
//!
//! Devices and backends are named. Collections of components of a given type
//! are stored in maps from names to component descriptions.
//!
//! # Verification
//!
//! This module has few opinions about what constitues a valid, usable spec: if
//! something deserializes, then as far as this module is concerned, it
//! describes a valid spec. Spec consumers, of course, will generally be more
//! discriminating, e.g. a Propolis server may refuse to start a VM that has
//! a device that names a nonexistent backend.
//!
//! # Versioning
//!
//! Instance spec versioning is not fully formalized yet. RFD 283 discusses some
//! possible approaches to formalizing it.
//!
//! What versioning requirements exist today are enforced through serde
//! attributes:
//!
//! - All components in an instance spec and the spec itself are marked
//! `#[serde(deny_unknown_fields)]`. This ensures that if spec version 2 adds a
//! new field, then it will not be interpreted by library version 1 as a v1 spec
//! unless library v2 removes the extra fields.
//! - New spec fields that have backward-compatible default values should have
//! the `#[serde(default)]` attribute so that previous spec versions can be
//! compatibly deserialized into new versions. If this isn't possible, the old
//! spec definition should be preserved so that library v2 can decide if it can
//! accept a v1 spec despite not being able to supply a default value for a new
//! field.

use std::collections::BTreeMap;
use std::convert::TryFrom;

use serde::{Deserialize, Serialize};
use thiserror::Error;

pub use crucible::VolumeConstructionRequest;
pub use propolis_types::PciPath;

/// Type alias for keys in the instance spec's maps.
type SpecKey = String;

/// An error type describing possible mismatches between two instance specs that
/// render them migration-incompatible.
#[derive(Debug, Error)]
pub enum SpecMismatch {
    #[error(
        "Specs have collections with different lengths (self: {0}, other: {1})"
    )]
    CollectionSize(usize, usize),

    #[error("Collection key {0} present in self but absent from other")]
    CollectionKeyAbsent(SpecKey),

    #[error(
        "Spec elements have different PCI paths (self: {0:?}, other: {1:?})"
    )]
    PciPath(PciPath, PciPath),

    #[error("Specs have different CPU counts (self: {0}, other: {1})")]
    CpuCount(u8, u8),

    #[error("Specs have different memory amounts (self: {0}, other: {1})")]
    MemorySize(u64, u64),

    #[error("Specs have different chipset types (self: {0:?}, other: {1:?})")]
    ChipsetType(Chipset, Chipset),

    #[error(
        "Specs have different PCIe chipset settings (self: {0}, other: {1})"
    )]
    PcieEnablement(bool, bool),

    #[error(
        "Storage backends have different kinds (self: {0:?}, other: {1:?})"
    )]
    StorageBackendKind(StorageBackendKind, StorageBackendKind),

    #[error(
        "Storage backends have different read-only settings \
        (self: {0}, other: {1})"
    )]
    StorageBackendReadonly(bool, bool),

    #[error(
        "Storage devices have different kinds (self: {0:?}, other: {1:?})"
    )]
    StorageDeviceKind(StorageDeviceKind, StorageDeviceKind),

    #[error(
        "Storage devices have different backend names (self: {0}, other: {1})"
    )]
    StorageDeviceBackend(String, String),

    #[error(
        "Network backends have different kinds (self: {0:?}, other: {1:?})"
    )]
    NetworkBackendKind(NetworkBackendKind, NetworkBackendKind),

    #[error(
        "Network devices have different backend names (self: {0}, other: {1})"
    )]
    NetworkDeviceBackend(String, String),

    #[error("Serial ports have different numbers (self: {0:?}, other: {1:?})")]
    SerialPortNumber(SerialPortNumber, SerialPortNumber),

    #[error(
        "PCI bridges have different downstream buses (self: {0}, other: {1})"
    )]
    PciBridgeDownstreamBus(u8, u8),

    #[cfg(test)]
    #[error("Test components differ")]
    TestComponents(),
}

/// Routines used to check whether two components are migration-compatible.
pub trait MigrationCompatible {
    /// Returns true if `self` and `other` describe spec elements that are
    /// similar enough to permit migration of this element from one VMM to
    /// another.
    ///
    /// Note that this can be, but isn't always, a simple check for equality.
    /// Backends, in particular, may be migration-compatible but have different
    /// configuration payloads. The migration protocol allows components like
    /// this to augment this check with their own compatibility checks.
    fn is_migration_compatible(&self, other: &Self)
        -> Result<(), SpecMismatch>;
}

impl<T: MigrationCompatible> MigrationCompatible for BTreeMap<SpecKey, T> {
    // Two keyed maps of components are compatible if they contain all the same
    // keys and if, for each key, the corresponding values are
    // migration-compatible.
    fn is_migration_compatible(
        &self,
        other: &Self,
    ) -> Result<(), SpecMismatch> {
        // If the two maps have different sizes, then they have different key
        // sets.
        if self.len() != other.len() {
            return Err(SpecMismatch::CollectionSize(self.len(), other.len()));
        }

        // Each key in `self`'s map must be present in `other`'s map, and the
        // corresponding values must be compatible with one another.
        for (key, this_val) in self.iter() {
            let other_val = other
                .get(key)
                .ok_or(SpecMismatch::CollectionKeyAbsent(key.clone()))?;

            this_val.is_migration_compatible(other_val)?;
        }

        Ok(())
    }
}

/// A kind of virtual chipset.
#[derive(Clone, Copy, Deserialize, Serialize, Debug, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub enum Chipset {
    /// An Intel 440FX-compatible chipset.
    I440Fx {
        /// Specifies whether the chipset should allow PCI configuration space
        /// to be accessed through the PCIe extended configuration mechanism.
        enable_pcie: bool,
    },
}

/// A VM's mainboard.
#[derive(Clone, Deserialize, Serialize, Debug, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Board {
    /// The number of virtual logical processors attached to this VM.
    pub cpus: u8,

    /// The amount of guest RAM attached to this VM.
    pub memory_mb: u64,

    /// The chipset to expose to guest software.
    pub chipset: Chipset,
    // TODO: Guest platform and CPU feature identification.
    // TODO: NUMA topology.
}

impl Default for Board {
    fn default() -> Self {
        Self {
            cpus: 0,
            memory_mb: 0,
            chipset: Chipset::I440Fx { enable_pcie: false },
        }
    }
}

impl MigrationCompatible for Board {
    fn is_migration_compatible(
        &self,
        other: &Self,
    ) -> Result<(), SpecMismatch> {
        if self.cpus != other.cpus {
            return Err(SpecMismatch::CpuCount(self.cpus, other.cpus));
        }
        if self.memory_mb != other.memory_mb {
            return Err(SpecMismatch::MemorySize(
                self.memory_mb,
                other.memory_mb,
            ));
        }
        match (self.chipset, other.chipset) {
            (
                Chipset::I440Fx { enable_pcie: this_enable },
                Chipset::I440Fx { enable_pcie: other_enable },
            ) => {
                if this_enable != other_enable {
                    return Err(SpecMismatch::PcieEnablement(
                        this_enable,
                        other_enable,
                    ));
                }
            }
        }

        Ok(())
    }
}

//
// Storage devices.
//

/// A description of a Crucible volume construction request.
#[derive(Clone, Deserialize, Serialize, Debug)]
pub struct CrucibleRequestContents {
    /// A [`crucible::VolumeConstructionRequest`], serialized as JSON.
    //
    // Storing volume construction requests in serialized form allows external
    // types to change without causing a breaking change to instance specs.
    // Consider the following scenario, assuming the VolumeConstructionRequest
    // struct is used directly:
    //
    // - Sled agent v1 starts Propolis v1 using Crucible request v1.
    // - Sled agent v2 (on some other sled with newer software) starts Propolis
    //   v2 using Crucible request v2, which has a new field not present in v1.
    // - Nexus orders a migration from v1 to v2. This requires someone to
    //   compare the two instances' specs for migratability.
    //
    // Migration compatibility is normally checked by the two Propolis servers
    // involved: one server sends its instance spec to the other, and the
    // recipient compares the specs to see if they're compatible. In this case,
    // v2 can't deserialize v1's spec (a field is missing), and v1 can't
    // deserialize v2's (an extra field is present), so migration will always
    // fail.
    //
    // Storing a serialized request avoids this problem as follows:
    //
    // - Sled agent v2 starts Propolis v2 with spec v2. It deserializes the
    //   request contents in the spec body into a v2 construction request.
    // - Migration begins. Propolis v2 can now deserialize the v1 instance spec
    //   and check it for compatibility. It can't deserialize the v1 *request
    //   contents*, but this can be dealt with separately (e.g. by having the v1
    //   and v2 Crucible components in the Propolis server negotiate
    //   compatibility themselves, which is an affordance the migration protocol
    //   allows).
    pub json: String,
}

impl TryFrom<&CrucibleRequestContents> for crucible::VolumeConstructionRequest {
    type Error = serde_json::Error;

    fn try_from(value: &CrucibleRequestContents) -> Result<Self, Self::Error> {
        serde_json::from_str(&value.json)
    }
}

impl TryFrom<&crucible::VolumeConstructionRequest> for CrucibleRequestContents {
    type Error = serde_json::Error;

    fn try_from(
        value: &crucible::VolumeConstructionRequest,
    ) -> Result<Self, Self::Error> {
        Ok(Self { json: serde_json::to_string(value)? })
    }
}

/// A kind of storage backend: a connection to on-sled resources or other
/// services that provide the functions storage devices need to implement their
/// contracts.
#[derive(Clone, Deserialize, Serialize, Debug)]
#[serde(deny_unknown_fields)]
pub enum StorageBackendKind {
    /// A Crucible-backed device, containing a generation number and a
    /// construction request.
    Crucible { gen: u64, req: CrucibleRequestContents },

    /// A device backed by a file on the host machine. The payload is a path to
    /// this file.
    File { path: String },

    /// A device backed by an in-memory buffer in the VMM process. The initial
    /// contents of this buffer are supplied out-of-band, either at
    /// instance initialization time or from a migration source.
    InMemory,
}

impl MigrationCompatible for StorageBackendKind {
    fn is_migration_compatible(
        &self,
        other: &Self,
    ) -> Result<(), SpecMismatch> {
        // Two storage backends are compatible if they have the same kind,
        // irrespective of their configurations. The migration protocol allows
        // individual backend instances to include messages in the preamble that
        // allow a source and target to decide independently whether they are
        // compatible with each other.
        if std::mem::discriminant(self) != std::mem::discriminant(other) {
            return Err(SpecMismatch::StorageBackendKind(
                self.clone(),
                other.clone(),
            ));
        }

        Ok(())
    }
}

/// A storage backend.
#[derive(Clone, Deserialize, Serialize, Debug)]
#[serde(deny_unknown_fields)]
pub struct StorageBackend {
    /// The kind of storage backend this is.
    pub kind: StorageBackendKind,

    /// Whether the storage is read-only.
    pub readonly: bool,
}

impl MigrationCompatible for StorageBackend {
    fn is_migration_compatible(
        &self,
        other: &Self,
    ) -> Result<(), SpecMismatch> {
        if self.readonly != other.readonly {
            return Err(SpecMismatch::StorageBackendReadonly(
                self.readonly,
                other.readonly,
            ));
        }
        self.kind.is_migration_compatible(&other.kind)
    }
}

/// A kind of storage device: the sort of virtual device interface the VMM
/// exposes to guest software.
#[derive(Clone, Copy, Deserialize, Serialize, Debug, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub enum StorageDeviceKind {
    Virtio,
    Nvme,
}

/// A storage device.
#[derive(Clone, Deserialize, Serialize, Debug, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct StorageDevice {
    /// The device interface to present to the guest.
    pub kind: StorageDeviceKind,

    /// The name of the device's backend.
    pub backend_name: String,

    /// The PCI path at which to attach this device.
    pub pci_path: PciPath,
}

impl MigrationCompatible for StorageDevice {
    fn is_migration_compatible(
        &self,
        other: &Self,
    ) -> Result<(), SpecMismatch> {
        if self.kind != other.kind {
            return Err(SpecMismatch::StorageDeviceKind(self.kind, other.kind));
        }
        if self.backend_name != other.backend_name {
            return Err(SpecMismatch::StorageDeviceBackend(
                self.backend_name.clone(),
                other.backend_name.clone(),
            ));
        }
        if self.pci_path != other.pci_path {
            return Err(SpecMismatch::PciPath(self.pci_path, other.pci_path));
        }
        Ok(())
    }
}

//
// Network devices.
//

/// A kind of network backend: a connection to an on-sled networking resource
/// that provides the functions needed for guest network adapters to implement
/// their contracts.
#[derive(Clone, Deserialize, Serialize, Debug)]
#[serde(deny_unknown_fields)]
pub enum NetworkBackendKind {
    /// A virtio-net (viona) backend associated with the supplied named vNIC on
    /// the host.
    Virtio { vnic_name: String },
}

impl MigrationCompatible for NetworkBackendKind {
    fn is_migration_compatible(
        &self,
        other: &Self,
    ) -> Result<(), SpecMismatch> {
        // Two network backends are compatible if they have the same kind,
        // irrespective of their configurations.
        if std::mem::discriminant(self) != std::mem::discriminant(other) {
            return Err(SpecMismatch::NetworkBackendKind(
                self.clone(),
                other.clone(),
            ));
        }

        Ok(())
    }
}

/// A network backend.
#[derive(Clone, Deserialize, Serialize, Debug)]
#[serde(deny_unknown_fields)]
pub struct NetworkBackend {
    pub kind: NetworkBackendKind,
}

impl MigrationCompatible for NetworkBackend {
    fn is_migration_compatible(
        &self,
        other: &Self,
    ) -> Result<(), SpecMismatch> {
        self.kind.is_migration_compatible(&other.kind)
    }
}

/// A virtual network adapter that presents a virtio network device interface.
#[derive(Clone, Deserialize, Serialize, Debug, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct NetworkDevice {
    /// The name of the device's backend.
    pub backend_name: String,

    /// The PCI path at which to attach this device.
    pub pci_path: PciPath,
}

impl MigrationCompatible for NetworkDevice {
    fn is_migration_compatible(
        &self,
        other: &Self,
    ) -> Result<(), SpecMismatch> {
        if self.backend_name != other.backend_name {
            return Err(SpecMismatch::NetworkDeviceBackend(
                self.backend_name.clone(),
                other.backend_name.clone(),
            ));
        }
        if self.pci_path != other.pci_path {
            return Err(SpecMismatch::PciPath(self.pci_path, other.pci_path));
        }

        Ok(())
    }
}

//
// Serial ports.
//

/// A serial port identifier, which determines what I/O ports a guest can use to
/// access a port.
#[derive(Clone, Copy, Deserialize, Serialize, Debug, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub enum SerialPortNumber {
    Com1,
    Com2,
    Com3,
    Com4,
}

/// A serial port device.
#[derive(Clone, Copy, Deserialize, Serialize, Debug, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SerialPort {
    /// The serial port number for this port.
    pub num: SerialPortNumber,
}

impl MigrationCompatible for SerialPort {
    fn is_migration_compatible(
        &self,
        other: &Self,
    ) -> Result<(), SpecMismatch> {
        if self.num != other.num {
            return Err(SpecMismatch::SerialPortNumber(self.num, other.num));
        }

        Ok(())
    }
}

/// A PCI-PCI bridge.
#[derive(Clone, Copy, Deserialize, Serialize, Debug, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PciPciBridge {
    /// The logical bus number of this bridge's downstream bus. Other devices
    /// may use this bus number in their PCI paths to indicate they should be
    /// attached to this bridge's bus.
    pub downstream_bus: u8,

    /// The PCI path at which to attach this bridge.
    pub pci_path: PciPath,
}

impl MigrationCompatible for PciPciBridge {
    fn is_migration_compatible(
        &self,
        other: &Self,
    ) -> Result<(), SpecMismatch> {
        if self.downstream_bus != other.downstream_bus {
            return Err(SpecMismatch::PciBridgeDownstreamBus(
                self.downstream_bus,
                other.downstream_bus,
            ));
        }
        if self.pci_path != other.pci_path {
            return Err(SpecMismatch::PciPath(self.pci_path, other.pci_path));
        }

        Ok(())
    }
}

/// A full instance specification. See the documentation for individual
/// elements for more information about the fields in this structure.
///
/// Named devices and backends are stored in maps with object names as keys
/// and devices/backends as values.
#[derive(Default, Clone, Deserialize, Serialize, Debug)]
#[serde(deny_unknown_fields)]
pub struct InstanceSpec {
    pub board: Board,

    pub storage_devices: BTreeMap<SpecKey, StorageDevice>,
    pub storage_backends: BTreeMap<SpecKey, StorageBackend>,
    pub network_devices: BTreeMap<SpecKey, NetworkDevice>,
    pub network_backends: BTreeMap<SpecKey, NetworkBackend>,
    pub serial_ports: BTreeMap<SpecKey, SerialPort>,
    pub pci_pci_bridges: BTreeMap<SpecKey, PciPciBridge>,
}

impl MigrationCompatible for InstanceSpec {
    fn is_migration_compatible(
        &self,
        other: &Self,
    ) -> Result<(), SpecMismatch> {
        self.board.is_migration_compatible(&other.board)?;
        self.storage_devices.is_migration_compatible(&other.storage_devices)?;
        self.storage_backends
            .is_migration_compatible(&other.storage_backends)?;
        self.network_devices.is_migration_compatible(&other.network_devices)?;
        self.network_backends
            .is_migration_compatible(&other.network_backends)?;
        self.serial_ports.is_migration_compatible(&other.serial_ports)?;
        self.pci_pci_bridges.is_migration_compatible(&other.pci_pci_bridges)?;

        Ok(())
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum TestComponent {
        Widget,
        Gizmo,
        Contraption,
    }

    impl MigrationCompatible for TestComponent {
        fn is_migration_compatible(
            &self,
            other: &Self,
        ) -> Result<(), SpecMismatch> {
            if self != other {
                Err(SpecMismatch::TestComponents())
            } else {
                Ok(())
            }
        }
    }

    // Verifies that the generic compatibility check for <key, component> maps
    // works correctly with a simple test type.
    #[test]
    fn generic_map_compatibility() {
        let m1: BTreeMap<SpecKey, TestComponent> = BTreeMap::from([
            ("widget".to_string(), TestComponent::Widget),
            ("gizmo".to_string(), TestComponent::Gizmo),
            ("contraption".to_string(), TestComponent::Contraption),
        ]);

        let mut m2 = m1.clone();
        assert!(m1.is_migration_compatible(&m2).is_ok());

        // Mismatched key counts make two maps incompatible.
        m2.insert("second_widget".to_string(), TestComponent::Widget);
        assert!(m1.is_migration_compatible(&m2).is_err());
        m2.remove("second_widget");

        // Two maps are incompatible if their keys refer to components that are
        // not compatible with each other.
        *m2.get_mut("gizmo").unwrap() = TestComponent::Contraption;
        assert!(m1.is_migration_compatible(&m2).is_err());
        *m2.get_mut("gizmo").unwrap() = TestComponent::Gizmo;

        // Two maps are incompatible if they have the same number of keys and
        // values, but different sets of key names.
        m2.remove("gizmo");
        m2.insert("other_gizmo".to_string(), TestComponent::Gizmo);
        assert!(m1.is_migration_compatible(&m2).is_err());
    }

    #[test]
    fn compatible_boards() {
        let b1 = Board {
            cpus: 8,
            memory_mb: 8192,
            chipset: Chipset::I440Fx { enable_pcie: false },
        };
        let b2 = b1.clone();
        assert!(b1.is_migration_compatible(&b2).is_ok());
    }

    #[test]
    fn incompatible_boards() {
        let b1 = Board {
            cpus: 4,
            memory_mb: 4096,
            chipset: Chipset::I440Fx { enable_pcie: true },
        };

        let mut b2 = b1.clone();
        b2.cpus = 8;
        assert!(matches!(
            b1.is_migration_compatible(&b2),
            Err(SpecMismatch::CpuCount(4, 8))
        ));
        b2.cpus = b1.cpus;

        b2.memory_mb = b1.memory_mb * 2;
        assert!(matches!(
            b1.is_migration_compatible(&b2),
            Err(SpecMismatch::MemorySize(4096, 8192))
        ));
        b2.memory_mb = b1.memory_mb;

        b2.chipset = Chipset::I440Fx { enable_pcie: false };
        assert!(matches!(
            b1.is_migration_compatible(&b2),
            Err(SpecMismatch::PcieEnablement(true, false))
        ));
    }

    #[test]
    fn compatible_storage_backends() {
        let b1: BTreeMap<SpecKey, StorageBackend> = BTreeMap::from([
            (
                "crucible".to_string(),
                StorageBackend {
                    kind: StorageBackendKind::Crucible {
                        gen: 1,
                        req: CrucibleRequestContents {
                            json: "this_crucible_config".to_string(),
                        },
                    },
                    readonly: true,
                },
            ),
            (
                "file".to_string(),
                StorageBackend {
                    kind: StorageBackendKind::File {
                        path: "this_path".to_string(),
                    },
                    readonly: false,
                },
            ),
            (
                "memory".to_string(),
                StorageBackend {
                    kind: StorageBackendKind::InMemory,
                    readonly: true,
                },
            ),
        ]);

        let mut b2 = b1.clone();
        match &mut b2.get_mut("crucible").unwrap().kind {
            StorageBackendKind::Crucible { gen, req } => {
                *gen += 1;
                *req = CrucibleRequestContents {
                    json: "that_crucible_config".to_string(),
                };
            }
            _ => panic!("Crucible backend not present in cloned map"),
        }
        assert!(b1.is_migration_compatible(&b2).is_ok());

        match &mut b2.get_mut("file").unwrap().kind {
            StorageBackendKind::File { path } => {
                *path = "that_path".to_string()
            }
            _ => panic!("File backend not present in cloned map"),
        }
        assert!(b1.is_migration_compatible(&b2).is_ok());
    }

    #[test]
    fn incompatible_storage_backends() {
        let b1 = StorageBackend {
            kind: StorageBackendKind::Crucible {
                gen: 1,
                req: CrucibleRequestContents { json: "config".to_string() },
            },
            readonly: true,
        };

        let mut b2 = b1.clone();
        b2.readonly = !b2.readonly;
        assert!(matches!(
            b1.is_migration_compatible(&b2),
            Err(SpecMismatch::StorageBackendReadonly(true, false))
        ));
        b2.readonly = b1.readonly;

        b2.kind = StorageBackendKind::File { path: "path".to_string() };
        assert!(matches!(
            b1.is_migration_compatible(&b2),
            Err(SpecMismatch::StorageBackendKind(
                StorageBackendKind::Crucible { .. },
                StorageBackendKind::File { .. }
            ))
        ));

        b2.kind = StorageBackendKind::InMemory;
        assert!(matches!(
            b1.is_migration_compatible(&b2),
            Err(SpecMismatch::StorageBackendKind(
                StorageBackendKind::Crucible { .. },
                StorageBackendKind::InMemory { .. }
            ))
        ));
    }

    #[test]
    fn compatible_storage_devices() {
        let d1 = StorageDevice {
            kind: StorageDeviceKind::Virtio,
            backend_name: "storage_backend".to_string(),
            pci_path: PciPath::new(0, 5, 0).unwrap(),
        };
        let d2 = d1.clone();
        assert!(d1.is_migration_compatible(&d2).is_ok());
    }

    #[test]
    fn incompatible_storage_devices() {
        let d1 = StorageDevice {
            kind: StorageDeviceKind::Virtio,
            backend_name: "storage_backend".to_string(),
            pci_path: PciPath::new(0, 5, 0).unwrap(),
        };

        let mut d2 = d1.clone();
        d2.kind = StorageDeviceKind::Nvme;
        assert!(matches!(
            d1.is_migration_compatible(&d2),
            Err(SpecMismatch::StorageDeviceKind(
                StorageDeviceKind::Virtio,
                StorageDeviceKind::Nvme
            ))
        ));
        d2.kind = d1.kind;

        d2.backend_name = "other_storage_backend".to_string();
        assert!(matches!(
            d1.is_migration_compatible(&d2),
            Err(SpecMismatch::StorageDeviceBackend(_, _))
        ));
        d2.backend_name = d1.backend_name.clone();

        d2.pci_path = PciPath::new(0, 6, 0).unwrap();
        assert!(matches!(
            d1.is_migration_compatible(&d2),
            Err(SpecMismatch::PciPath(_, _))
        ));
    }

    #[test]
    fn compatible_network_devices() {
        let n1 = NetworkDevice {
            backend_name: "net_backend".to_string(),
            pci_path: PciPath::new(0, 7, 0).unwrap(),
        };
        let n2 = n1.clone();
        assert!(n1.is_migration_compatible(&n2).is_ok());
    }

    #[test]
    fn incompatible_network_devices() {
        let n1 = NetworkDevice {
            backend_name: "net_backend".to_string(),
            pci_path: PciPath::new(0, 7, 0).unwrap(),
        };
        let mut n2 = n1.clone();

        n2.backend_name = "other_net_backend".to_string();
        assert!(matches!(
            n1.is_migration_compatible(&n2),
            Err(SpecMismatch::NetworkDeviceBackend(_, _))
        ));
        n2.backend_name = n1.backend_name.clone();

        n2.pci_path = PciPath::new(0, 8, 1).unwrap();
        assert!(matches!(
            n1.is_migration_compatible(&n2),
            Err(SpecMismatch::PciPath(_, _))
        ));
    }

    #[test]
    fn compatible_network_backends() {
        let n1 = NetworkBackend {
            kind: NetworkBackendKind::Virtio { vnic_name: "vnic".to_string() },
        };
        let n2 = NetworkBackend {
            kind: NetworkBackendKind::Virtio {
                vnic_name: "other_vnic".to_string(),
            },
        };
        assert!(n1.is_migration_compatible(&n2).is_ok());
    }

    #[test]
    fn serial_port_compatibility() {
        assert!((SerialPort { num: SerialPortNumber::Com1 })
            .is_migration_compatible(&SerialPort {
                num: SerialPortNumber::Com1
            })
            .is_ok());
        assert!(matches!(
            (SerialPort { num: SerialPortNumber::Com2 })
                .is_migration_compatible(&SerialPort {
                    num: SerialPortNumber::Com3
                }),
            Err(SpecMismatch::SerialPortNumber(
                SerialPortNumber::Com2,
                SerialPortNumber::Com3
            ))
        ));
    }

    #[test]
    fn pci_bridge_compatibility() {
        let b1 = PciPciBridge {
            downstream_bus: 1,
            pci_path: PciPath::new(1, 2, 3).unwrap(),
        };

        let mut b2 = b1.clone();
        assert!(b1.is_migration_compatible(&b2).is_ok());

        b2.downstream_bus += 1;
        assert!(matches!(
            b1.is_migration_compatible(&b2),
            Err(SpecMismatch::PciBridgeDownstreamBus(1, 2))
        ));
        b2.downstream_bus = b1.downstream_bus;

        b2.pci_path = PciPath::new(4, 5, 6).unwrap();
        assert!(matches!(
            b1.is_migration_compatible(&b2),
            Err(SpecMismatch::PciPath(_, _))
        ));
    }
}
