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

use serde::{Deserialize, Serialize};

pub use propolis_types::PciPath;

/// A kind of virtual chipset.
#[derive(Clone, Copy, Deserialize, Serialize, Debug)]
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
#[derive(Clone, Deserialize, Serialize, Debug)]
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

//
// Storage devices.
//

/// A kind of storage backend: a connection to on-sled resources or other
/// services that provide the functions storage devices need to implement their
/// contracts.
#[derive(Clone, Deserialize, Serialize, Debug)]
#[serde(deny_unknown_fields)]
pub enum StorageBackendKind {
    /// A Crucible-backed device, containing a generation number and a
    /// serialized [`crucible::VolumeConstructionRequest`].
    Crucible { gen: u64, serialized_req: String },

    /// A device backed by a file on the host machine. The payload is a path to
    /// this file.
    File { path: String },

    /// A device backed by an in-memory buffer in the VMM process. The payload
    /// is the buffer's contents.
    InMemory { bytes: Vec<u8> },
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

/// A kind of storage device: the sort of virtual device interface the VMM
/// exposes to guest software.
#[derive(Clone, Copy, Deserialize, Serialize, Debug)]
#[serde(deny_unknown_fields)]
pub enum StorageDeviceKind {
    Virtio,
    Nvme,
}

/// A storage device.
#[derive(Clone, Deserialize, Serialize, Debug)]
#[serde(deny_unknown_fields)]
pub struct StorageDevice {
    /// The device interface to present to the guest.
    pub kind: StorageDeviceKind,

    /// The name of the device's backend.
    pub backend_name: String,

    /// The PCI path at which to attach this device.
    pub pci_path: PciPath,
}

//
// Network devices.
//
// Because there is only one kind of virtual networking device (a virtio-net
// device with an enlightened vNIC), networking devices don't have the same
// BackendKind and DeviceKind enums that storage devices do, but they can
// be added if needed in future versions.

/// A network backend, specifically a virtual NIC on the host system for which
/// Virtio network adapter integration is enabled.
#[derive(Clone, Deserialize, Serialize, Debug)]
#[serde(deny_unknown_fields)]
pub struct NetworkBackend {
    /// The name of the vNIC to connect to.
    pub vnic_name: String,
}

/// A virtual network adapter that presents a virtio network device interface.
#[derive(Clone, Deserialize, Serialize, Debug)]
#[serde(deny_unknown_fields)]
pub struct NetworkDevice {
    /// The name of the device's backend.
    pub backend_name: String,

    /// The PCI path at which to attach this device.
    pub pci_path: PciPath,
}

//
// Serial ports.
//

/// A serial port identifier, which determines what I/O ports a guest can use to
/// access a port.
#[derive(Clone, Copy, Deserialize, Serialize, Debug)]
#[serde(deny_unknown_fields)]
pub enum SerialPortNumber {
    Com1,
    Com2,
    Com3,
    Com4,
}

/// A serial port device.
#[derive(Clone, Copy, Deserialize, Serialize, Debug)]
#[serde(deny_unknown_fields)]
pub struct SerialPort {
    /// The serial port number for this port.
    pub num: SerialPortNumber,

    /// The initial discard discipline for this port. If true, the device will
    /// not buffer bytes written from the guest until the port's discipline
    /// changes.
    pub autodiscard: bool,
}

/// A PCI-PCI bridge.
#[derive(Clone, Copy, Deserialize, Serialize, Debug)]
#[serde(deny_unknown_fields)]
pub struct PciPciBridge {
    /// The logical bus number of this bridge's downstream bus. Other devices
    /// may use this bus number in their PCI paths to indicate they should be
    /// attached to this bridge's bus.
    pub downstream_bus: u8,

    /// The PCI path at which to attach this bridge.
    pub pci_path: PciPath,
}

/// A full instance specification. See the documentation for individual
/// elements for more information about the fields in this structure.
///
/// Named devices and backends are stored in maps with object names as keys
/// and devices/backends as values.
#[derive(Clone, Deserialize, Serialize, Debug)]
#[serde(deny_unknown_fields)]
pub struct InstanceSpec {
    pub board: Board,

    pub storage_devices: BTreeMap<String, StorageDevice>,
    pub storage_backends: BTreeMap<String, StorageBackend>,
    pub network_devices: BTreeMap<String, NetworkDevice>,
    pub network_backends: BTreeMap<String, NetworkBackend>,
    pub serial_ports: BTreeMap<String, SerialPort>,
    pub pci_pci_bridges: BTreeMap<String, PciPciBridge>,
}
