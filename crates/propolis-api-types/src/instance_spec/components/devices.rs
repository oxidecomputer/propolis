// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

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

    /// A caller-defined correlation identifier for this interface. If Propolis
    /// is configured to collect network interface kstats in its Oximeter
    /// metrics, the metric series for this interface will be associated with
    /// this identifier.
    pub interface_id: uuid::Uuid,

    /// The PCI path at which to attach this device.
    pub pci_path: PciPath,
}

/// A serial port identifier, which determines what I/O ports a guest can use to
/// access a port.
#[derive(
    Clone, Copy, Deserialize, Serialize, Debug, PartialEq, Eq, JsonSchema, Hash,
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

#[derive(
    Clone,
    Copy,
    Deserialize,
    Serialize,
    Debug,
    PartialEq,
    Eq,
    JsonSchema,
    Default,
)]
#[serde(deny_unknown_fields)]
pub struct QemuPvpanic {
    /// Enable the QEMU PVPANIC ISA bus device (I/O port 0x505).
    pub enable_isa: bool,
    // TODO(eliza): add support for the PCI PVPANIC device...
}

/// Settings supplied to the guest's firmware image that specify the order in
/// which it should consider its options when selecting a device to try to boot
/// from.
#[derive(Clone, Deserialize, Serialize, Debug, JsonSchema, Default)]
#[serde(deny_unknown_fields)]
pub struct BootSettings {
    /// An ordered list of components to attempt to boot from.
    pub order: Vec<BootOrderEntry>,
}

/// An entry in the boot order stored in a [`BootSettings`] component.
#[derive(Clone, Deserialize, Serialize, Debug, JsonSchema, Default)]
pub struct BootOrderEntry {
    /// The name of another component in the spec that Propolis should try to
    /// boot from.
    ///
    /// Currently, only disk device components are supported.
    pub name: String,
}

//
// Structs for Falcon devices. These devices don't support live migration.
//

/// Describes a SoftNPU PCI device.
///
/// This is only supported by Propolis servers compiled with the `falcon`
/// feature.
#[derive(Clone, Deserialize, Serialize, Debug, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SoftNpuPciPort {
    /// The PCI path at which to attach the guest to this port.
    pub pci_path: PciPath,
}

/// Describes a SoftNPU network port.
///
/// This is only supported by Propolis servers compiled with the `falcon`
/// feature.
#[derive(Clone, Deserialize, Serialize, Debug, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SoftNpuPort {
    /// The name of the SoftNpu port.
    pub name: String,

    /// The name of the device's backend.
    pub backend_name: String,
}

/// Describes a PCI device that shares host files with the guest using the P9
/// protocol.
///
/// This is only supported by Propolis servers compiled with the `falcon`
/// feature.
#[derive(Clone, Deserialize, Serialize, Debug, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SoftNpuP9 {
    /// The PCI path at which to attach the guest to this port.
    pub pci_path: PciPath,
}

/// Describes a filesystem to expose through a P9 device.
///
/// This is only supported by Propolis servers compiled with the `falcon`
/// feature.
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
