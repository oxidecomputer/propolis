//! The "device" portions of an instance spec: the parts of the spec that define
//! VM properties that are visible to guest software.
//!
//! # Foreign types & versioning
//!
//! Propolis expects to be able to send [`DeviceSpec`] structures between
//! instances to ensure that they describe the same guest-visible components.
//! (This is part of the live migration protocol described in RFD 71.) This
//! applies to Propolis version changes in both directions:
//!
//! - Propolis upgrade: Newer versions of Propolis must be able to deserialize
//!   and interpret device specs constructed by earlier versions.
//! - Propolis downgrade: Older versions of Propolis must be able to deserialize
//!   and interpret device specs constructed by newer versions, even if the
//!   newer versions contain fields not known to the older versions. This can be
//!   done by explicitly versioning the device spec structure using enum
//!   variants or by using optional/default-able fields that can be skipped with
//!   the `skip_serializing_if` attribute.
//!
//! Because types defined outside this crate can change without heeding these
//! requirements, device specs should use only types defined in the crate or
//! built-in Rust types.

use std::collections::BTreeMap;

use super::{
    ElementCompatibilityError, MigrationCollection,
    MigrationCompatibilityError, MigrationElement, PciPath, SpecKey,
};
use serde::{Deserialize, Serialize};

//
// Mainboard definitions: a VM's chipset, CPUs, and memory.
//

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

impl MigrationElement for Board {
    fn can_migrate_from_element(
        &self,
        other: &Self,
    ) -> Result<(), ElementCompatibilityError> {
        if self.cpus != other.cpus {
            return Err(ElementCompatibilityError::CpuCount(
                self.cpus, other.cpus,
            ));
        }
        if self.memory_mb != other.memory_mb {
            return Err(ElementCompatibilityError::MemorySize(
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
                    return Err(ElementCompatibilityError::PcieEnablement(
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

impl MigrationElement for StorageDevice {
    fn can_migrate_from_element(
        &self,
        other: &Self,
    ) -> Result<(), ElementCompatibilityError> {
        if self.kind != other.kind {
            return Err(ElementCompatibilityError::StorageDeviceKind(
                self.kind, other.kind,
            ));
        }
        if self.backend_name != other.backend_name {
            return Err(ElementCompatibilityError::StorageDeviceBackend(
                self.backend_name.clone(),
                other.backend_name.clone(),
            ));
        }
        if self.pci_path != other.pci_path {
            return Err(ElementCompatibilityError::PciPath(
                self.pci_path,
                other.pci_path,
            ));
        }
        Ok(())
    }
}

//
// Network devices.
//

/// A virtual network adapter that presents a virtio network device interface.
#[derive(Clone, Deserialize, Serialize, Debug, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct NetworkDevice {
    /// The name of the device's backend.
    pub backend_name: String,

    /// The PCI path at which to attach this device.
    pub pci_path: PciPath,
}

impl MigrationElement for NetworkDevice {
    fn can_migrate_from_element(
        &self,
        other: &Self,
    ) -> Result<(), ElementCompatibilityError> {
        if self.backend_name != other.backend_name {
            return Err(ElementCompatibilityError::NetworkDeviceBackend(
                self.backend_name.clone(),
                other.backend_name.clone(),
            ));
        }
        if self.pci_path != other.pci_path {
            return Err(ElementCompatibilityError::PciPath(
                self.pci_path,
                other.pci_path,
            ));
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

impl MigrationElement for SerialPort {
    fn can_migrate_from_element(
        &self,
        other: &Self,
    ) -> Result<(), ElementCompatibilityError> {
        if self.num != other.num {
            return Err(ElementCompatibilityError::SerialPortNumber(
                self.num, other.num,
            ));
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

impl MigrationElement for PciPciBridge {
    fn can_migrate_from_element(
        &self,
        other: &Self,
    ) -> Result<(), ElementCompatibilityError> {
        if self.downstream_bus != other.downstream_bus {
            return Err(ElementCompatibilityError::PciBridgeDownstreamBus(
                self.downstream_bus,
                other.downstream_bus,
            ));
        }
        if self.pci_path != other.pci_path {
            return Err(ElementCompatibilityError::PciPath(
                self.pci_path,
                other.pci_path,
            ));
        }

        Ok(())
    }
}

#[cfg(feature = "falcon")]
#[derive(Clone, Deserialize, Serialize, Debug)]
#[serde(deny_unknown_fields)]
pub struct SoftNpuPciPort {
    /// The PCI path at which to attach the guest to this port.
    pub pci_path: PciPath,
}

#[cfg(feature = "falcon")]
#[derive(Clone, Deserialize, Serialize, Debug)]
#[serde(deny_unknown_fields)]
pub struct SoftNpuPort {
    /// The name of the SoftNPU port.
    pub name: String,

    /// The name of the associated VNIC.
    pub vnic: String,
}

#[cfg(feature = "falcon")]
#[derive(Clone, Deserialize, Serialize, Debug)]
#[serde(deny_unknown_fields)]
pub struct SoftNpuP9 {
    /// The PCI path at which to attach the guest to this port.
    pub pci_path: PciPath,
}

#[cfg(feature = "falcon")]
#[derive(Clone, Deserialize, Serialize, Debug)]
#[serde(deny_unknown_fields)]
pub struct P9fs {
    pub source: String,
    pub target: String,
    pub chunk_size: u32,
    pub pci_path: PciPath,
}

#[derive(Default, Clone, Deserialize, Serialize, Debug)]
#[serde(deny_unknown_fields)]
pub struct DeviceSpec {
    pub board: Board,
    pub storage_devices: BTreeMap<SpecKey, StorageDevice>,
    pub network_devices: BTreeMap<SpecKey, NetworkDevice>,
    pub serial_ports: BTreeMap<SpecKey, SerialPort>,
    pub pci_pci_bridges: BTreeMap<SpecKey, PciPciBridge>,
    #[cfg(feature = "falcon")]
    pub softnpu_pci_port: Option<SoftNpuPciPort>,
    #[cfg(feature = "falcon")]
    pub softnpu_ports: BTreeMap<SpecKey, SoftNpuPort>,
    #[cfg(feature = "falcon")]
    pub softnpu_p9: Option<SoftNpuP9>,
    #[cfg(feature = "falcon")]
    pub p9fs: Option<P9fs>,
}

impl DeviceSpec {
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
                    "network devices".to_string(),
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
