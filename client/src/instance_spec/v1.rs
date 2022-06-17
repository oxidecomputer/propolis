use std::collections::BTreeMap;

use crate::instance_spec::common::PciPath;

use serde::{Deserialize, Serialize};

/// A kind of virtual chipset.
#[derive(Deserialize, Serialize, Debug)]
pub enum Chipset {
    /// An Intel 440FX-compatible chipset.
    I440Fx { enable_pcie: bool },
}

/// A VM's mainboard.
#[derive(Deserialize, Serialize, Debug)]
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
#[derive(Deserialize, Serialize, Debug)]
pub enum StorageBackendKind {
    /// A Crucible-backed device, containing a generation number and a
    /// serialized [`crucible::VolumeConstructionRequest`] stored as a string
    /// (so that changes to the construction request don't change the format of
    /// this payload).
    Crucible { gen: u64, serialized_req: String },

    /// A device backed by a file on the host machine. The payload is a path to
    /// this file.
    File { path: String },

    /// A device backed by an in-memory buffer in the VMM process. The payload
    /// is the buffer's contents.
    InMemory { bytes: Vec<u8> },
}

/// A storage backend.
#[derive(Deserialize, Serialize, Debug)]
pub struct StorageBackend {
    /// The kind of storage backend this is.
    pub kind: StorageBackendKind,

    /// Whether the storage is read-only.
    pub readonly: bool,
}

/// A kind of storage device: the sort of virtual device interface the VMM
/// exposes to guest software.
#[derive(Deserialize, Serialize, Debug)]
pub enum StorageDeviceKind {
    Virtio,
    Nvme,
}

/// A storage device.
#[derive(Deserialize, Serialize, Debug)]
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
#[derive(Deserialize, Serialize, Debug)]
pub struct NetworkBackend {
    /// The name of the vNIC to connect to.
    pub vnic_name: String,
}

#[derive(Deserialize, Serialize, Debug)]
pub struct NetworkDevice {
    /// The name of the device's backend.
    pub backend_name: String,

    /// The PCI path at which to attach this device.
    pub pci_path: PciPath,
}

//
// Serial ports.
//

/// The number of the serial port, which determines what I/O ports the guest
/// can use to access it.
#[derive(Deserialize, Serialize, Debug)]
pub enum SerialPortNumber {
    Com1,
    Com2,
    Com3,
    Com4,
}

/// A serial port device.
#[derive(Deserialize, Serialize, Debug)]
pub struct SerialPort {
    /// The serial port number for this port.
    pub num: SerialPortNumber,

    /// The initial discard discipline for this port. If true, the device will
    /// not buffer bytes written from the guest until the port's discipline
    /// changes.
    pub autodiscard: bool,
}

/// A PCI-PCI bridge.
#[derive(Deserialize, Serialize, Debug)]
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
#[derive(Deserialize, Serialize, Debug)]
pub struct InstanceSpec {
    pub board: Board,

    pub storage_devices: BTreeMap<String, StorageDevice>,
    pub storage_backends: BTreeMap<String, StorageBackend>,
    pub network_devices: BTreeMap<String, NetworkDevice>,
    pub network_backends: BTreeMap<String, NetworkBackend>,
    pub serial_ports: BTreeMap<String, SerialPort>,
    pub pci_pci_bridges: BTreeMap<String, PciPciBridge>,
}
