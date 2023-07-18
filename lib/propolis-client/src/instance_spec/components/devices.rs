//! Device configuration data: components that define VM properties that are
//! visible to a VM's guest software.

use crate::instance_spec::PciPath;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// A disk that presents a virtio-block interface to the guest.
#[derive(Clone, Deserialize, Serialize, Debug, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct VirtioDisk {
    /// The name of the disk's backend component.
    pub backend_name: String,

    /// The PCI bus/device/function at which this disk should be attached.
    pub pci_path: PciPath,
}

/// A disk that presents an NVMe interface to the guest.
#[derive(Clone, Deserialize, Serialize, Debug, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NvmeDisk {
    /// The name of the disk's backend component.
    pub backend_name: String,

    /// The PCI bus/device/function at which this disk should be attached.
    pub pci_path: PciPath,
}

/// A network card that presents a virtio-net interface to the guest.
#[derive(Clone, Deserialize, Serialize, Debug, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct VirtioNic {
    /// The name of the device's backend.
    pub backend_name: String,

    /// The PCI path at which to attach this device.
    pub pci_path: PciPath,
}

/// A serial port identifier, which determines what I/O ports a guest can use to
/// access a port.
#[derive(
    Clone, Copy, Deserialize, Serialize, Debug, PartialEq, Eq, JsonSchema,
)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub enum SerialPortNumber {
    Com1,
    Com2,
    Com3,
    Com4,
}

/// A serial port device.
#[derive(
    Clone, Copy, Deserialize, Serialize, Debug, PartialEq, Eq, JsonSchema,
)]
#[serde(deny_unknown_fields)]
pub struct SerialPort {
    /// The serial port number for this port.
    pub num: SerialPortNumber,
}

/// A PCI-PCI bridge.
#[derive(
    Clone, Copy, Deserialize, Serialize, Debug, PartialEq, Eq, JsonSchema,
)]
#[serde(deny_unknown_fields)]
pub struct PciPciBridge {
    /// The logical bus number of this bridge's downstream bus. Other devices
    /// may use this bus number in their PCI paths to indicate they should be
    /// attached to this bridge's bus.
    pub downstream_bus: u8,

    /// The PCI path at which to attach this bridge.
    pub pci_path: PciPath,
}

#[cfg(feature = "falcon")]
#[derive(Clone, Deserialize, Serialize, Debug, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SoftNpuPciPort {
    /// The PCI path at which to attach the guest to this port.
    pub pci_path: PciPath,
}

#[cfg(feature = "falcon")]
#[derive(Clone, Deserialize, Serialize, Debug, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SoftNpuPort {
    /// The name of the SoftNpu port.
    pub name: String,

    /// The name of the device's backend.
    pub backend_name: String,
}

#[cfg(feature = "falcon")]
#[derive(Clone, Deserialize, Serialize, Debug, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SoftNpuP9 {
    /// The PCI path at which to attach the guest to this port.
    pub pci_path: PciPath,
}

#[cfg(feature = "falcon")]
#[derive(Clone, Deserialize, Serialize, Debug, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct P9fs {
    /// The host source path to mount into the guest.
    pub source: String,

    /// The 9P target filesystem tag.
    pub target: String,

    /// The chunk size to use in the 9P protocol. Vanilla Helios images should
    /// use 8192. Falcon Helios base images and Linux can use up to 65536.
    pub chunk_size: u32,

    /// The PCI path at which to attach the guest to this P9 filesystem.
    pub pci_path: PciPath,
}
