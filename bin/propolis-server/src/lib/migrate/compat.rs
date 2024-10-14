// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Associated functions for the [`crate::spec::Spec`] type that determine
//! whether two specs describe migration-compatible VMs.

use std::collections::HashMap;

use crate::spec::{self, SerialPortDevice};

use cpuid_utils::CpuidVendor;
use propolis_api_types::instance_spec::{
    components::{
        board::Chipset,
        devices::{PciPciBridge, SerialPortNumber},
    },
    PciPath,
};
use thiserror::Error;

trait CompatCheck {
    type Error;

    fn is_compatible_with(&self, other: &Self) -> Result<(), Self::Error>;
}

#[derive(Debug, Error)]
pub enum CompatibilityError {
    #[error(transparent)]
    Board(#[from] BoardIncompatibility),

    #[error(transparent)]
    Pvpanic(#[from] PvpanicIncompatibility),

    #[error("collection {0} incompatible")]
    Collection(String, #[source] CollectionIncompatibility),

    #[cfg(feature = "falcon")]
    #[error("can't migrate instances containing softnpu devices")]
    SoftNpu,
}

#[derive(Debug, Error)]
pub enum BoardIncompatibility {
    #[error("boards have different CPU counts (self: {this}, other: {other})")]
    CpuCount { this: u8, other: u8 },

    #[error(
        "boards have different memory sizes (self: {this}, other: {other})"
    )]
    MemorySize { this: u64, other: u64 },

    #[error(
        "chipsets have different PCIe settings (self: {this}, other: {other})"
    )]
    PcieEnabled { this: bool, other: bool },

    #[error(transparent)]
    Cpuid(#[from] CpuidMismatch),
}

#[derive(Debug, Error)]
pub enum CpuidMismatch {
    #[error("CPUID is explicit in one spec but not the other (self: {this}, other: {other}")]
    Explicitness { this: bool, other: bool },

    #[error(
        "CPUID sets have different CPU vendors (self: {this}, other: {other})"
    )]
    Vendor { this: CpuidVendor, other: CpuidVendor },

    #[error(transparent)]
    LeavesOrValues(#[from] cpuid_utils::CpuidSetMismatch),
}

#[derive(Debug, Error)]
pub enum DiskIncompatibility {
    #[error(
        "disks have different device interfaces (self: {this}, other: {other})"
    )]
    Interface { this: &'static str, other: &'static str },

    #[error("disks have different PCI paths (self: {this}, other: {other})")]
    PciPath { this: PciPath, other: PciPath },

    #[error(
        "disks have different backend names (self: {this:?}, other: {other:?})"
    )]
    BackendName { this: String, other: String },

    #[error(
        "disks have different backend kinds (self: {this}, other: {other})"
    )]
    BackendKind { this: &'static str, other: &'static str },

    #[error(
        "disks have different read-only settings (self: {this}, other: {other})"
    )]
    ReadOnly { this: bool, other: bool },
}

#[derive(Debug, Error)]
pub enum NicIncompatibility {
    #[error("NICs have different PCI paths (self: {this}, other: {other})")]
    PciPath { this: PciPath, other: PciPath },

    #[error(
        "NICs have different backend names (self: {this}, other: {other})"
    )]
    BackendName { this: String, other: String },
}

#[derive(Debug, Error)]
pub enum SerialPortIncompatibility {
    #[error("ports have different numbers (self: {this:?}, other: {other:?})")]
    Number { this: SerialPortNumber, other: SerialPortNumber },

    #[error("ports have different devices (self: {this}, other: {other})")]
    Device { this: SerialPortDevice, other: SerialPortDevice },
}

#[derive(Debug, Error)]
pub enum BridgeIncompatibility {
    #[error("bridges have different PCI paths (self: {this}, other: {other})")]
    PciPath { this: PciPath, other: PciPath },

    #[error("bridges have different downstream buses (self: {this}, other: {other})")]
    DownstreamBus { this: u8, other: u8 },
}

#[derive(Debug, Error)]
pub enum PvpanicIncompatibility {
    #[error("pvpanic presence differs (self: {this}, other: {other})")]
    Presence { this: bool, other: bool },

    #[error(
        "pvpanic devices have different names (self: {this:?}, other: {other:?})"
    )]
    Name { this: String, other: String },

    #[error(
        "pvpanic devices have different ISA settings (self: {this}, other: {other})"
    )]
    EnableIsa { this: bool, other: bool },
}

#[derive(Debug, Error)]
pub enum ComponentIncompatibility {
    #[error(transparent)]
    Board(#[from] BoardIncompatibility),

    #[error(transparent)]
    Disk(#[from] DiskIncompatibility),

    #[error(transparent)]
    Nic(#[from] NicIncompatibility),

    #[error(transparent)]
    SerialPort(#[from] SerialPortIncompatibility),

    #[error(transparent)]
    PciPciBridge(#[from] BridgeIncompatibility),
}

#[derive(Debug, Error)]
pub enum CollectionIncompatibility {
    #[error(
        "collections have different lengths (self: {this}, other: {other})"
    )]
    Length { this: usize, other: usize },

    #[error("collection key {0} present in self but not other")]
    KeyAbsent(String),

    #[error("component {0} incompatible")]
    Component(String, #[source] ComponentIncompatibility),
}

impl<T: CompatCheck<Error = ComponentIncompatibility>> CompatCheck
    for HashMap<String, T>
{
    type Error = CollectionIncompatibility;

    fn is_compatible_with(
        &self,
        other: &Self,
    ) -> Result<(), CollectionIncompatibility> {
        if self.len() != other.len() {
            return Err(CollectionIncompatibility::Length {
                this: self.len(),
                other: other.len(),
            });
        }

        for (key, this_val) in self.iter() {
            let other_val = other.get(key).ok_or_else(|| {
                CollectionIncompatibility::KeyAbsent(key.clone())
            })?;

            this_val.is_compatible_with(other_val).map_err(|e| {
                CollectionIncompatibility::Component(key.clone(), e)
            })?;
        }

        Ok(())
    }
}

impl spec::Spec {
    fn is_board_compatible(
        &self,
        other: &Self,
    ) -> Result<(), BoardIncompatibility> {
        self.is_chipset_compatible(other)?;
        self.is_cpuid_compatible(other)?;

        let this = &self.board;
        let other = &other.board;
        if this.cpus != other.cpus {
            Err(BoardIncompatibility::CpuCount {
                this: this.cpus,
                other: other.cpus,
            })
        } else if this.memory_mb != other.memory_mb {
            Err(BoardIncompatibility::MemorySize {
                this: this.memory_mb,
                other: other.memory_mb,
            })
        } else {
            Ok(())
        }
    }

    fn is_chipset_compatible(
        &self,
        other: &Self,
    ) -> Result<(), BoardIncompatibility> {
        let Chipset::I440Fx(this) = self.board.chipset;
        let Chipset::I440Fx(other) = other.board.chipset;

        if this.enable_pcie != other.enable_pcie {
            Err(BoardIncompatibility::PcieEnabled {
                this: this.enable_pcie,
                other: other.enable_pcie,
            })
        } else {
            Ok(())
        }
    }

    fn is_cpuid_compatible(&self, other: &Self) -> Result<(), CpuidMismatch> {
        match (&self.cpuid, &other.cpuid) {
            (None, None) => Ok(()),
            (Some(_), None) | (None, Some(_)) => {
                Err(CpuidMismatch::Explicitness {
                    this: self.cpuid.is_some(),
                    other: other.cpuid.is_some(),
                })
            }
            (Some(this), Some(other)) => {
                if this.vendor() != other.vendor() {
                    return Err(CpuidMismatch::Vendor {
                        this: this.vendor(),
                        other: other.vendor(),
                    });
                }

                this.is_equivalent_to(other)?;
                Ok(())
            }
        }
    }

    fn is_pvpanic_compatible(
        &self,
        other: &Self,
    ) -> Result<(), PvpanicIncompatibility> {
        match (&self.pvpanic, &other.pvpanic) {
            (None, None) => Ok(()),
            (Some(this), Some(other)) if this.name != other.name => {
                Err(PvpanicIncompatibility::Name {
                    this: this.name.clone(),
                    other: other.name.clone(),
                })
            }
            (Some(this), Some(other))
                if this.spec.enable_isa != other.spec.enable_isa =>
            {
                Err(PvpanicIncompatibility::EnableIsa {
                    this: this.spec.enable_isa,
                    other: other.spec.enable_isa,
                })
            }
            (Some(_), Some(_)) => Ok(()),
            (this, other) => Err(PvpanicIncompatibility::Presence {
                this: this.is_some(),
                other: other.is_some(),
            }),
        }
    }

    pub(super) fn is_migration_compatible(
        &self,
        other: &Self,
    ) -> Result<(), CompatibilityError> {
        self.is_board_compatible(other)?;
        self.disks.is_compatible_with(&other.disks).map_err(|e| {
            CompatibilityError::Collection("disks".to_string(), e)
        })?;

        self.nics.is_compatible_with(&other.nics).map_err(|e| {
            CompatibilityError::Collection("nics".to_string(), e)
        })?;

        self.serial.is_compatible_with(&other.serial).map_err(|e| {
            CompatibilityError::Collection("serial ports".to_string(), e)
        })?;

        self.pci_pci_bridges
            .is_compatible_with(&other.pci_pci_bridges)
            .map_err(|e| {
                CompatibilityError::Collection("PCI bridges".to_string(), e)
            })?;

        self.is_pvpanic_compatible(other)?;

        #[cfg(feature = "falcon")]
        if self.softnpu.has_components() || other.softnpu.has_components() {
            return Err(CompatibilityError::SoftNpu);
        }

        Ok(())
    }
}

impl spec::StorageDevice {
    fn is_compatible_with(
        &self,
        other: &Self,
    ) -> Result<(), DiskIncompatibility> {
        if std::mem::discriminant(self) != std::mem::discriminant(other) {
            Err(DiskIncompatibility::Interface {
                this: self.kind(),
                other: other.kind(),
            })
        } else if self.pci_path() != other.pci_path() {
            Err(DiskIncompatibility::PciPath {
                this: self.pci_path(),
                other: other.pci_path(),
            })
        } else if self.backend_name() != other.backend_name() {
            Err(DiskIncompatibility::BackendName {
                this: self.backend_name().to_owned(),
                other: other.backend_name().to_owned(),
            })
        } else {
            Ok(())
        }
    }
}

impl spec::StorageBackend {
    fn is_compatible_with(
        &self,
        other: &Self,
    ) -> Result<(), DiskIncompatibility> {
        if std::mem::discriminant(self) != std::mem::discriminant(other) {
            Err(DiskIncompatibility::BackendKind {
                this: self.kind(),
                other: other.kind(),
            })
        } else if self.read_only() != other.read_only() {
            Err(DiskIncompatibility::ReadOnly {
                this: self.read_only(),
                other: other.read_only(),
            })
        } else {
            Ok(())
        }
    }
}

impl CompatCheck for spec::Disk {
    type Error = ComponentIncompatibility;

    fn is_compatible_with(
        &self,
        other: &Self,
    ) -> Result<(), ComponentIncompatibility> {
        self.device_spec.is_compatible_with(&other.device_spec)?;
        self.backend_spec.is_compatible_with(&other.backend_spec)?;
        Ok(())
    }
}

impl CompatCheck for spec::Nic {
    type Error = ComponentIncompatibility;

    fn is_compatible_with(
        &self,
        other: &Self,
    ) -> Result<(), ComponentIncompatibility> {
        if self.device_spec.pci_path != other.device_spec.pci_path {
            Err(NicIncompatibility::PciPath {
                this: self.device_spec.pci_path,
                other: other.device_spec.pci_path,
            })
        } else if self.device_spec.backend_name
            != other.device_spec.backend_name
        {
            Err(NicIncompatibility::BackendName {
                this: self.device_spec.backend_name.clone(),
                other: other.device_spec.backend_name.clone(),
            })
        } else {
            Ok(())
        }
        .map_err(ComponentIncompatibility::Nic)
    }
}

impl CompatCheck for spec::SerialPort {
    type Error = ComponentIncompatibility;

    fn is_compatible_with(
        &self,
        other: &Self,
    ) -> Result<(), ComponentIncompatibility> {
        if self.num != other.num {
            Err(SerialPortIncompatibility::Number {
                this: self.num,
                other: other.num,
            })
        } else if std::mem::discriminant(&self.device)
            != std::mem::discriminant(&other.device)
        {
            Err(SerialPortIncompatibility::Device {
                this: self.device,
                other: other.device,
            })
        } else {
            Ok(())
        }
        .map_err(ComponentIncompatibility::SerialPort)
    }
}

impl CompatCheck for PciPciBridge {
    type Error = ComponentIncompatibility;

    fn is_compatible_with(
        &self,
        other: &Self,
    ) -> Result<(), ComponentIncompatibility> {
        if self.pci_path != other.pci_path {
            Err(BridgeIncompatibility::PciPath {
                this: self.pci_path,
                other: other.pci_path,
            })
        } else if self.downstream_bus != other.downstream_bus {
            Err(BridgeIncompatibility::DownstreamBus {
                this: self.downstream_bus,
                other: other.downstream_bus,
            })
        } else {
            Ok(())
        }
        .map_err(ComponentIncompatibility::PciPciBridge)
    }
}

#[cfg(test)]
mod test {
    use cpuid_utils::{CpuidIdent, CpuidSet, CpuidValues};
    use propolis_api_types::instance_spec::components::{
        backends::{
            CrucibleStorageBackend, FileStorageBackend, VirtioNetworkBackend,
        },
        board::I440Fx,
        devices::{
            NvmeDisk, QemuPvpanic as QemuPvpanicDesc, VirtioDisk, VirtioNic,
        },
    };
    use spec::{QemuPvpanic, StorageDevice};
    use uuid::Uuid;

    use super::*;

    fn new_spec() -> spec::Spec {
        let mut spec = spec::Spec::default();
        spec.board.cpus = 2;
        spec.board.memory_mb = 512;
        spec
    }

    fn file_backend() -> spec::StorageBackend {
        spec::StorageBackend::File(FileStorageBackend {
            path: "/tmp/file.raw".to_owned(),
            readonly: false,
        })
    }

    fn crucible_backend() -> spec::StorageBackend {
        spec::StorageBackend::Crucible(CrucibleStorageBackend {
            request_json: "{}".to_owned(),
            readonly: false,
        })
    }

    fn nic() -> spec::Nic {
        spec::Nic {
            device_spec: VirtioNic {
                pci_path: PciPath::new(0, 16, 0).unwrap(),
                interface_id: Uuid::new_v4(),
                backend_name: "vnic".to_owned(),
            },
            backend_spec: VirtioNetworkBackend {
                vnic_name: "vnic0".to_owned(),
            },
        }
    }

    fn serial_port() -> spec::SerialPort {
        spec::SerialPort {
            num: SerialPortNumber::Com1,
            device: SerialPortDevice::Uart,
        }
    }

    fn bridge() -> PciPciBridge {
        PciPciBridge {
            downstream_bus: 1,
            pci_path: PciPath::new(0, 24, 0).unwrap(),
        }
    }

    #[test]
    fn cpu_mismatch() {
        let s1 = new_spec();
        let mut s2 = s1.clone();
        s2.board.cpus += 1;
        assert!(s1.is_migration_compatible(&s2).is_err());
    }

    #[test]
    fn memory_mismatch() {
        let s1 = new_spec();
        let mut s2 = s1.clone();
        s2.board.memory_mb *= 2;
        assert!(s1.is_migration_compatible(&s2).is_err());
    }

    #[test]
    fn pcie_mismatch() {
        let mut s1 = new_spec();
        let mut s2 = s1.clone();
        s1.board.chipset = Chipset::I440Fx(I440Fx { enable_pcie: false });
        s2.board.chipset = Chipset::I440Fx(I440Fx { enable_pcie: true });
        assert!(s1.is_migration_compatible(&s2).is_err());
    }

    #[test]
    fn pvpanic_name_mismatch() {
        let mut s1 = new_spec();
        s1.pvpanic = Some(QemuPvpanic {
            name: "pvpanic1".to_string(),
            spec: QemuPvpanicDesc { enable_isa: true },
        });
        let mut s2 = s1.clone();
        s2.pvpanic.as_mut().unwrap().name = "pvpanic2".to_string();
        assert!(s1.is_migration_compatible(&s2).is_err());
    }

    #[test]
    fn pvpanic_enable_isa_mismatch() {
        let mut s1 = new_spec();
        s1.pvpanic = Some(QemuPvpanic {
            name: "pvpanic".to_string(),
            spec: QemuPvpanicDesc { enable_isa: true },
        });
        let mut s2 = s1.clone();
        s2.pvpanic.as_mut().unwrap().spec.enable_isa = false;
        assert!(s1.is_migration_compatible(&s2).is_err());
    }

    #[test]
    fn compatible_disks() {
        let mut s1 = new_spec();
        let mut s2 = s1.clone();
        let disk = spec::Disk {
            device_spec: StorageDevice::Virtio(VirtioDisk {
                pci_path: PciPath::new(0, 4, 0).unwrap(),
                backend_name: "backend".to_string(),
            }),
            backend_spec: crucible_backend(),
        };

        s1.disks.insert("disk".to_owned(), disk.clone());
        s2.disks.insert("disk".to_owned(), disk);
        assert!(s1.is_migration_compatible(&s2).is_ok());
    }

    #[test]
    fn disk_name_mismatch() {
        let mut s1 = new_spec();
        let mut s2 = s1.clone();
        let d1 = spec::Disk {
            device_spec: StorageDevice::Virtio(VirtioDisk {
                pci_path: PciPath::new(0, 4, 0).unwrap(),
                backend_name: "backend".to_owned(),
            }),
            backend_spec: crucible_backend(),
        };

        s1.disks.insert("disk1".to_owned(), d1.clone());
        s2.disks.insert("disk2".to_owned(), d1);
        assert!(s1.is_migration_compatible(&s2).is_err());
    }

    #[test]
    fn disk_length_mismatch() {
        let mut s1 = new_spec();
        let mut s2 = s1.clone();
        let d1 = spec::Disk {
            device_spec: StorageDevice::Virtio(VirtioDisk {
                pci_path: PciPath::new(0, 4, 0).unwrap(),
                backend_name: "backend".to_owned(),
            }),
            backend_spec: crucible_backend(),
        };

        s1.disks.insert("disk1".to_owned(), d1.clone());
        s2.disks.insert("disk1".to_owned(), d1.clone());
        s2.disks.insert("disk2".to_owned(), d1);
        assert!(s1.is_migration_compatible(&s2).is_err());
    }

    #[test]
    fn disk_interface_mismatch() {
        let mut s1 = new_spec();
        let mut s2 = s1.clone();
        let d1 = spec::Disk {
            device_spec: StorageDevice::Virtio(VirtioDisk {
                pci_path: PciPath::new(0, 4, 0).unwrap(),
                backend_name: "backend".to_owned(),
            }),
            backend_spec: crucible_backend(),
        };

        let mut d2 = d1.clone();
        d2.device_spec = StorageDevice::Nvme(NvmeDisk {
            pci_path: PciPath::new(0, 4, 0).unwrap(),
            backend_name: "backend".to_owned(),
        });

        s1.disks.insert("disk".to_owned(), d1);
        s2.disks.insert("disk".to_owned(), d2);
        assert!(s1.is_migration_compatible(&s2).is_err());
    }

    #[test]
    fn disk_pci_mismatch() {
        let mut s1 = new_spec();
        let mut s2 = s1.clone();
        let d1 = spec::Disk {
            device_spec: StorageDevice::Virtio(VirtioDisk {
                pci_path: PciPath::new(0, 4, 0).unwrap(),
                backend_name: "backend".to_owned(),
            }),
            backend_spec: crucible_backend(),
        };

        let mut d2 = d1.clone();
        d2.device_spec = StorageDevice::Virtio(VirtioDisk {
            pci_path: PciPath::new(0, 5, 0).unwrap(),
            backend_name: "backend".to_owned(),
        });

        s1.disks.insert("disk".to_owned(), d1);
        s2.disks.insert("disk".to_owned(), d2);
        assert!(s1.is_migration_compatible(&s2).is_err());
    }

    #[test]
    fn disk_backend_name_mismatch() {
        let mut s1 = new_spec();
        let mut s2 = s1.clone();
        let d1 = spec::Disk {
            device_spec: StorageDevice::Virtio(VirtioDisk {
                pci_path: PciPath::new(0, 4, 0).unwrap(),
                backend_name: "backend".to_owned(),
            }),
            backend_spec: crucible_backend(),
        };

        let mut d2 = d1.clone();
        d2.device_spec = StorageDevice::Virtio(VirtioDisk {
            pci_path: PciPath::new(0, 4, 0).unwrap(),
            backend_name: "other_backend".to_owned(),
        });

        s1.disks.insert("disk".to_owned(), d1);
        s2.disks.insert("disk".to_owned(), d2);
        assert!(s1.is_migration_compatible(&s2).is_err());
    }

    #[test]
    fn disk_backend_kind_mismatch() {
        let mut s1 = new_spec();
        let mut s2 = s1.clone();
        let d1 = spec::Disk {
            device_spec: StorageDevice::Virtio(VirtioDisk {
                pci_path: PciPath::new(0, 4, 0).unwrap(),
                backend_name: "backend".to_owned(),
            }),
            backend_spec: file_backend(),
        };

        let mut d2 = d1.clone();
        d2.backend_spec = crucible_backend();
        s1.disks.insert("disk".to_owned(), d1);
        s2.disks.insert("disk".to_owned(), d2);
        assert!(s1.is_migration_compatible(&s2).is_err());
    }

    #[test]
    fn disk_backend_readonly_mismatch() {
        let mut s1 = new_spec();
        let mut s2 = s1.clone();
        let d1 = spec::Disk {
            device_spec: StorageDevice::Virtio(VirtioDisk {
                pci_path: PciPath::new(0, 4, 0).unwrap(),
                backend_name: "backend".to_owned(),
            }),
            backend_spec: file_backend(),
        };

        let mut d2 = d1.clone();
        d2.backend_spec = spec::StorageBackend::File(FileStorageBackend {
            path: "/tmp/file.raw".to_owned(),
            readonly: true,
        });

        s1.disks.insert("disk".to_owned(), d1);
        s2.disks.insert("disk".to_owned(), d2);
        assert!(s1.is_migration_compatible(&s2).is_err());
    }

    #[test]
    fn compatible_nics() {
        let mut s1 = new_spec();
        let mut s2 = s1.clone();
        let nic = nic();
        s1.nics.insert("nic".to_owned(), nic.clone());
        s2.nics.insert("nic".to_owned(), nic);
        assert!(s1.is_migration_compatible(&s2).is_ok());
    }

    #[test]
    fn nic_pci_mismatch() {
        let mut s1 = new_spec();
        let mut s2 = s1.clone();
        let n1 = nic();
        let mut n2 = n1.clone();
        n2.device_spec.pci_path = PciPath::new(0, 24, 0).unwrap();
        s1.nics.insert("nic".to_owned(), n1);
        s2.nics.insert("nic".to_owned(), n2);
        assert!(s1.is_migration_compatible(&s2).is_err());
    }

    #[test]
    fn nic_backend_name_mismatch() {
        let mut s1 = new_spec();
        let mut s2 = s1.clone();
        let n1 = nic();
        let mut n2 = n1.clone();
        "other_backend".clone_into(&mut n2.device_spec.backend_name);
        s1.nics.insert("nic".to_owned(), n1);
        s2.nics.insert("nic".to_owned(), n2);
        assert!(s1.is_migration_compatible(&s2).is_err());
    }

    #[test]
    fn compatible_serial_ports() {
        let mut s1 = new_spec();
        let mut s2 = s1.clone();
        let serial = serial_port();
        s1.serial.insert("com1".to_owned(), serial.clone());
        s2.serial.insert("com1".to_owned(), serial);
        assert!(s1.is_migration_compatible(&s2).is_ok());
    }

    #[test]
    fn serial_port_number_mismatch() {
        let mut s1 = new_spec();
        let mut s2 = s1.clone();
        let serial1 = serial_port();
        let mut serial2 = serial1.clone();
        serial2.num = SerialPortNumber::Com2;
        s1.serial.insert("com1".to_owned(), serial1);
        s2.serial.insert("com1".to_owned(), serial2);
        assert!(s1.is_migration_compatible(&s2).is_err());
    }

    #[test]
    fn compatible_bridges() {
        let mut s1 = new_spec();
        let mut s2 = s1.clone();
        let bridge = bridge();
        s1.pci_pci_bridges.insert("bridge1".to_owned(), bridge);
        s2.pci_pci_bridges.insert("bridge1".to_owned(), bridge);
        assert!(s1.is_migration_compatible(&s2).is_ok());
    }

    #[test]
    fn bridge_downstream_bus_mismatch() {
        let mut s1 = new_spec();
        let mut s2 = s1.clone();
        let b1 = bridge();
        let mut b2 = b1;
        b2.downstream_bus += 1;
        s1.pci_pci_bridges.insert("bridge1".to_owned(), b1);
        s2.pci_pci_bridges.insert("bridge1".to_owned(), b2);
        assert!(s1.is_migration_compatible(&s2).is_err());
    }

    #[test]
    fn bridge_pci_path_mismatch() {
        let mut s1 = new_spec();
        let mut s2 = s1.clone();
        let b1 = bridge();
        let mut b2 = b1;
        b2.pci_path = PciPath::new(0, 30, 0).unwrap();
        s1.pci_pci_bridges.insert("bridge1".to_owned(), b1);
        s2.pci_pci_bridges.insert("bridge1".to_owned(), b2);
        assert!(s1.is_migration_compatible(&s2).is_err());
    }

    #[test]
    fn compatible_cpuid() {
        let mut s1 = new_spec();
        let mut s2 = s1.clone();
        let mut set1 = CpuidSet::new(CpuidVendor::Intel);
        let mut set2 = CpuidSet::new(CpuidVendor::Intel);

        s1.cpuid = Some(set1.clone());
        s2.cpuid = Some(set2.clone());
        s1.is_migration_compatible(&s2).unwrap();

        set1.insert(CpuidIdent::leaf(0x1337), CpuidValues::default()).unwrap();
        set2.insert(CpuidIdent::leaf(0x1337), CpuidValues::default()).unwrap();

        s1.cpuid = Some(set1.clone());
        s2.cpuid = Some(set2.clone());
        s1.is_migration_compatible(&s2).unwrap();

        let values = CpuidValues { eax: 5, ebx: 6, ecx: 7, edx: 8 };
        set1.insert(CpuidIdent::subleaf(3, 4), values).unwrap();
        set2.insert(CpuidIdent::subleaf(3, 4), values).unwrap();
        s1.is_migration_compatible(&s2).unwrap();
    }

    #[test]
    fn cpuid_explicitness_mismatch() {
        let mut s1 = new_spec();
        let s2 = s1.clone();
        s1.cpuid = Some(CpuidSet::new(CpuidVendor::Intel));
        assert!(s1.is_migration_compatible(&s2).is_err());
    }

    #[test]
    fn cpuid_vendor_mismatch() {
        let mut s1 = new_spec();
        let mut s2 = s1.clone();
        s1.cpuid = Some(CpuidSet::new(CpuidVendor::Intel));
        s2.cpuid = Some(CpuidSet::new(CpuidVendor::Amd));
        assert!(s1.is_migration_compatible(&s2).is_err());
    }

    #[test]
    fn cpuid_leaf_set_mismatch() {
        let mut s1 = new_spec();
        let mut s2 = s1.clone();
        let mut set1 = CpuidSet::new(CpuidVendor::Amd);
        let mut set2 = CpuidSet::new(CpuidVendor::Amd);

        // Give the first set an entry the second set doesn't have.
        set1.insert(CpuidIdent::leaf(0), CpuidValues::default()).unwrap();
        set1.insert(CpuidIdent::leaf(1), CpuidValues::default()).unwrap();
        set2.insert(CpuidIdent::leaf(0), CpuidValues::default()).unwrap();

        s1.cpuid = Some(set1);
        s2.cpuid = Some(set2.clone());
        assert!(s1.is_migration_compatible(&s2).is_err());

        // Make the sets have the same number of entries, but with a difference
        // in which entries they have.
        set2.insert(CpuidIdent::leaf(3), CpuidValues::default()).unwrap();
        s2.cpuid = Some(set2);
        assert!(s1.is_migration_compatible(&s2).is_err());
    }

    #[test]
    fn cpuid_leaf_value_mismatch() {
        let mut s1 = new_spec();
        let mut s2 = s1.clone();
        let mut set1 = CpuidSet::new(CpuidVendor::Amd);
        let mut set2 = CpuidSet::new(CpuidVendor::Amd);

        let v1 = CpuidValues { eax: 4, ebx: 5, ecx: 6, edx: 7 };
        let v2 = CpuidValues { eax: 100, ebx: 200, ecx: 300, edx: 400 };
        set1.insert(CpuidIdent::leaf(0), v1).unwrap();
        set2.insert(CpuidIdent::leaf(0), v2).unwrap();
        s1.cpuid = Some(set1);
        s2.cpuid = Some(set2);
        assert!(s1.is_migration_compatible(&s2).is_err());
    }

    #[test]
    fn cpuid_leaf_subleaf_conflict() {
        let mut s1 = new_spec();
        let mut s2 = s1.clone();
        let mut set1 = CpuidSet::new(CpuidVendor::Amd);
        let mut set2 = CpuidSet::new(CpuidVendor::Amd);

        // Check that leaf 0 with no subleaf is not compatible with leaf 0 and a
        // subleaf of 0. These are semantically different: the former matches
        // leaf 0 with any subleaf value, while the latter technically matches
        // only leaf 0 and subleaf 0 (with leaf-specific behavior if a different
        // subleaf is specified).
        set1.insert(CpuidIdent::leaf(0), CpuidValues::default()).unwrap();
        set2.insert(CpuidIdent::subleaf(0, 0), CpuidValues::default()).unwrap();
        s1.cpuid = Some(set1);
        s2.cpuid = Some(set2);
        assert!(s1.is_migration_compatible(&s2).is_err());
    }
}
