//! The "device" portions of an instance spec: the parts of the spec that define
//! VM properties that are visible to guest software.

use std::collections::BTreeMap;

use super::{MigrationCompatible, PciPath, SpecKey, SpecMismatchDetails};
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

impl MigrationCompatible for Board {
    fn is_migration_compatible(
        &self,
        other: &Self,
    ) -> Result<(), SpecMismatchDetails> {
        if self.cpus != other.cpus {
            return Err(SpecMismatchDetails::CpuCount(self.cpus, other.cpus));
        }
        if self.memory_mb != other.memory_mb {
            return Err(SpecMismatchDetails::MemorySize(
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
                    return Err(SpecMismatchDetails::PcieEnablement(
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

impl MigrationCompatible for StorageDevice {
    fn is_migration_compatible(
        &self,
        other: &Self,
    ) -> Result<(), SpecMismatchDetails> {
        if self.kind != other.kind {
            return Err(SpecMismatchDetails::StorageDeviceKind(
                self.kind, other.kind,
            ));
        }
        if self.backend_name != other.backend_name {
            return Err(SpecMismatchDetails::StorageDeviceBackend(
                self.backend_name.clone(),
                other.backend_name.clone(),
            ));
        }
        if self.pci_path != other.pci_path {
            return Err(SpecMismatchDetails::PciPath(
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

impl MigrationCompatible for NetworkDevice {
    fn is_migration_compatible(
        &self,
        other: &Self,
    ) -> Result<(), SpecMismatchDetails> {
        if self.backend_name != other.backend_name {
            return Err(SpecMismatchDetails::NetworkDeviceBackend(
                self.backend_name.clone(),
                other.backend_name.clone(),
            ));
        }
        if self.pci_path != other.pci_path {
            return Err(SpecMismatchDetails::PciPath(
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

impl MigrationCompatible for SerialPort {
    fn is_migration_compatible(
        &self,
        other: &Self,
    ) -> Result<(), SpecMismatchDetails> {
        if self.num != other.num {
            return Err(SpecMismatchDetails::SerialPortNumber(
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

impl MigrationCompatible for PciPciBridge {
    fn is_migration_compatible(
        &self,
        other: &Self,
    ) -> Result<(), SpecMismatchDetails> {
        if self.downstream_bus != other.downstream_bus {
            return Err(SpecMismatchDetails::PciBridgeDownstreamBus(
                self.downstream_bus,
                other.downstream_bus,
            ));
        }
        if self.pci_path != other.pci_path {
            return Err(SpecMismatchDetails::PciPath(
                self.pci_path,
                other.pci_path,
            ));
        }

        Ok(())
    }
}

#[derive(Default, Clone, Deserialize, Serialize, Debug)]
#[serde(deny_unknown_fields)]
pub struct DeviceSpec {
    pub board: Board,
    pub storage_devices: BTreeMap<SpecKey, StorageDevice>,
    pub network_devices: BTreeMap<SpecKey, NetworkDevice>,
    pub serial_ports: BTreeMap<SpecKey, SerialPort>,
    pub pci_pci_bridges: BTreeMap<SpecKey, PciPciBridge>,
}

impl DeviceSpec {
    pub fn is_migration_compatible(
        &self,
        other: &Self,
    ) -> Result<(), (String, SpecMismatchDetails)> {
        self.board
            .is_migration_compatible(&other.board)
            .map_err(|e| ("board".to_string(), e))?;

        self.storage_devices
            .is_migration_compatible(&other.storage_devices)
            .map_err(|e| ("storage devices".to_string(), e))?;

        self.network_devices
            .is_migration_compatible(&other.network_devices)
            .map_err(|e| ("network devices".to_string(), e))?;

        self.serial_ports
            .is_migration_compatible(&other.serial_ports)
            .map_err(|e| ("serial ports".to_string(), e))?;

        self.pci_pci_bridges
            .is_migration_compatible(&other.pci_pci_bridges)
            .map_err(|e| ("PCI bridges".to_string(), e))?;

        Ok(())
    }
}
