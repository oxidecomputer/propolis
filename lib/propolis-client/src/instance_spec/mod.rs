//! Instance specifications: abstract descriptions of a VM's devices and config.
//!
//! An instance spec describes a VM's virtual devices, backends, and other
//! guest environment configuration supplied by the Propolis VMM. RFD 283
//! contains more details about how specs are used throughout the Oxide stack.
//!
//! # Spec format
//!
//! Instance specs are divided into two parts:
//!
//! - The "device" half describes the VM components and interfaces that guest
//!   software can observe directly. Device configuration generally can't change
//!   at runtime without the guest's cooperation.
//! - The "backend" half describes how the VM connects to services (provided by
//!   the host OS or other parts of the rack) that supply functions the devices
//!   need to provide their abstractions to guests.
//!
//! For example, to expose a virtual NVMe disk to a guest, a spec defines an
//! NVMe device (expressing that the VMM should create a PCI device exposing an
//! NVMe-conforming interface at a specific bus/device/function) and connects it
//! to a storage backend (expressing that I/O to the virtual disk should be
//! serviced by a local file, or by the Crucible storage service, or by a buffer
//! in the VMM's memory).
//!
//! # Instance specs and the VM lifecycle
//!
//! Instance specs are used to initialize new VMs and during live migration.
//! VM initialization uses specs to determine what components to create. Live
//! migration uses the [`MigrationCompatible`] trait to determine whether two
//! specs would, if realized, create VMs that are sufficiently compatible to
//! allow one VM to migrate to the other.
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
//! NOTE: Instance spec versioning is not fully formalized yet; see RFD 283.
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

use serde::{Deserialize, Serialize};
use thiserror::Error;

pub use crucible_client_types::VolumeConstructionRequest;
pub use propolis_types::PciPath;

mod backends;
mod devices;

pub use backends::*;
pub use devices::*;

/// Type alias for keys in the instance spec's maps.
type SpecKey = String;

/// An error type describing possible mismatches between two instance specs that
/// render them migration-incompatible.
#[derive(Debug, Error)]
pub enum SpecMismatchDetails {
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

#[derive(Debug, Error)]
pub enum MigrationCompatibilityError {
    #[error("Migration of {0} not compatible: {1}")]
    SpecMismatch(String, SpecMismatchDetails),
}

/// Routines used to check whether two components are migration-compatible.
trait MigrationCompatible {
    /// Returns true if `self` and `other` describe spec elements that are
    /// similar enough to permit migration of this element from one VMM to
    /// another.
    ///
    /// Note that this can be, but isn't always, a simple check for equality.
    /// Backends, in particular, may be migration-compatible but have different
    /// configuration payloads. The migration protocol allows components like
    /// this to augment this check with their own compatibility checks.
    fn is_migration_compatible(
        &self,
        other: &Self,
    ) -> Result<(), SpecMismatchDetails>;
}

impl<T: MigrationCompatible> MigrationCompatible for BTreeMap<SpecKey, T> {
    // Two keyed maps of components are compatible if they contain all the same
    // keys and if, for each key, the corresponding values are
    // migration-compatible.
    fn is_migration_compatible(
        &self,
        other: &Self,
    ) -> Result<(), SpecMismatchDetails> {
        // If the two maps have different sizes, then they have different key
        // sets.
        if self.len() != other.len() {
            return Err(SpecMismatchDetails::CollectionSize(
                self.len(),
                other.len(),
            ));
        }

        // Each key in `self`'s map must be present in `other`'s map, and the
        // corresponding values must be compatible with one another.
        for (key, this_val) in self.iter() {
            let other_val = other.get(key).ok_or_else(|| {
                SpecMismatchDetails::CollectionKeyAbsent(key.clone())
            })?;

            this_val.is_migration_compatible(other_val)?;
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
    pub devices: DeviceSpec,
    pub backends: BackendSpec,
}

impl InstanceSpec {
    pub fn is_migration_compatible(
        &self,
        other: &Self,
    ) -> Result<(), MigrationCompatibilityError> {
        self.devices
            .is_migration_compatible(&other.devices)
            .map_err(|e| MigrationCompatibilityError::SpecMismatch(e.0, e.1))?;

        self.backends
            .is_migration_compatible(&other.backends)
            .map_err(|e| MigrationCompatibilityError::SpecMismatch(e.0, e.1))?;

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
        ) -> Result<(), SpecMismatchDetails> {
            if self != other {
                Err(SpecMismatchDetails::TestComponents())
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
            Err(SpecMismatchDetails::CpuCount(4, 8))
        ));
        b2.cpus = b1.cpus;

        b2.memory_mb = b1.memory_mb * 2;
        assert!(matches!(
            b1.is_migration_compatible(&b2),
            Err(SpecMismatchDetails::MemorySize(4096, 8192))
        ));
        b2.memory_mb = b1.memory_mb;

        b2.chipset = Chipset::I440Fx { enable_pcie: false };
        assert!(matches!(
            b1.is_migration_compatible(&b2),
            Err(SpecMismatchDetails::PcieEnablement(true, false))
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
            Err(SpecMismatchDetails::StorageBackendReadonly(true, false))
        ));
        b2.readonly = b1.readonly;

        b2.kind = StorageBackendKind::File { path: "path".to_string() };
        assert!(matches!(
            b1.is_migration_compatible(&b2),
            Err(SpecMismatchDetails::StorageBackendKind(
                StorageBackendKind::Crucible { .. },
                StorageBackendKind::File { .. }
            ))
        ));

        b2.kind = StorageBackendKind::InMemory;
        assert!(matches!(
            b1.is_migration_compatible(&b2),
            Err(SpecMismatchDetails::StorageBackendKind(
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
            Err(SpecMismatchDetails::StorageDeviceKind(
                StorageDeviceKind::Virtio,
                StorageDeviceKind::Nvme
            ))
        ));
        d2.kind = d1.kind;

        d2.backend_name = "other_storage_backend".to_string();
        assert!(matches!(
            d1.is_migration_compatible(&d2),
            Err(SpecMismatchDetails::StorageDeviceBackend(_, _))
        ));
        d2.backend_name = d1.backend_name.clone();

        d2.pci_path = PciPath::new(0, 6, 0).unwrap();
        assert!(matches!(
            d1.is_migration_compatible(&d2),
            Err(SpecMismatchDetails::PciPath(_, _))
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
            Err(SpecMismatchDetails::NetworkDeviceBackend(_, _))
        ));
        n2.backend_name = n1.backend_name.clone();

        n2.pci_path = PciPath::new(0, 8, 1).unwrap();
        assert!(matches!(
            n1.is_migration_compatible(&n2),
            Err(SpecMismatchDetails::PciPath(_, _))
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
            Err(SpecMismatchDetails::SerialPortNumber(
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
            Err(SpecMismatchDetails::PciBridgeDownstreamBus(1, 2))
        ));
        b2.downstream_bus = b1.downstream_bus;

        b2.pci_path = PciPath::new(4, 5, 6).unwrap();
        assert!(matches!(
            b1.is_migration_compatible(&b2),
            Err(SpecMismatchDetails::PciPath(_, _))
        ));
    }
}
