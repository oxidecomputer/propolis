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

pub use crucible::VolumeConstructionRequest;
pub use propolis_types::PciPath;

/// Type alias for keys in the instance spec's maps.
type SpecKey = String;

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
    fn is_migration_compatible(&self, other: &Self) -> bool;
}

impl<T: MigrationCompatible> MigrationCompatible for BTreeMap<SpecKey, T> {
    // Two keyed maps of components are compatible if they contain all the same
    // keys and if, for each key, the corresponding values are
    // migration-compatible.
    fn is_migration_compatible(&self, other: &Self) -> bool {
        // If the two maps have different sizes, then they have different key
        // sets.
        if self.len() != other.len() {
            return false;
        }

        // Each key in `self`'s map must be present in `other`'s map, and the
        // corresponding values must be compatible with one another.
        for (key, this_val) in self.iter() {
            match other.get(key) {
                Some(other_val) => {
                    if !this_val.is_migration_compatible(other_val) {
                        return false;
                    }
                }
                None => return false,
            }
        }

        true
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
    fn is_migration_compatible(&self, other: &Self) -> bool {
        self == other
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
    fn is_migration_compatible(&self, other: &Self) -> bool {
        // Two storage backends are compatible if they have the same kind,
        // irrespective of their configurations.
        std::mem::discriminant(self) == std::mem::discriminant(other)
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
    fn is_migration_compatible(&self, other: &Self) -> bool {
        self.readonly == other.readonly
            && self.kind.is_migration_compatible(&other.kind)
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
    fn is_migration_compatible(&self, other: &Self) -> bool {
        self == other
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
    fn is_migration_compatible(&self, other: &Self) -> bool {
        // Two network backends are compatible if they have the same kind,
        // irrespective of their configurations.
        std::mem::discriminant(self) == std::mem::discriminant(other)
    }
}

/// A network backend.
#[derive(Clone, Deserialize, Serialize, Debug)]
#[serde(deny_unknown_fields)]
pub struct NetworkBackend {
    pub kind: NetworkBackendKind,
}

impl MigrationCompatible for NetworkBackend {
    fn is_migration_compatible(&self, other: &Self) -> bool {
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
    fn is_migration_compatible(&self, other: &Self) -> bool {
        self == other
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
    fn is_migration_compatible(&self, other: &Self) -> bool {
        self == other
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
    fn is_migration_compatible(&self, other: &Self) -> bool {
        self == other
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
    fn is_migration_compatible(&self, other: &Self) -> bool {
        self.board.is_migration_compatible(&other.board)
            && self
                .storage_devices
                .is_migration_compatible(&other.storage_devices)
            && self
                .storage_backends
                .is_migration_compatible(&other.storage_backends)
            && self
                .network_devices
                .is_migration_compatible(&other.network_devices)
            && self
                .network_backends
                .is_migration_compatible(&other.network_backends)
            && self.serial_ports.is_migration_compatible(&other.serial_ports)
            && self
                .pci_pci_bridges
                .is_migration_compatible(&other.pci_pci_bridges)
    }
}
