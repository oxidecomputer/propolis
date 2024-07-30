// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Checks for compatibility of two instance specs.

use std::collections::HashMap;

use crate::spec::{self, SerialPortUser};

use propolis_api_types::instance_spec::{
    components::{
        board::Chipset,
        devices::{PciPciBridge, SerialPortNumber},
    },
    PciPath,
};
use thiserror::Error;

trait CompatComponent {
    fn is_compatible_with(
        &self,
        other: &Self,
    ) -> Result<(), ComponentIncompatibility>;
}

trait CompatCollection {
    fn is_compatible_with(
        &self,
        other: &Self,
    ) -> Result<(), CollectionIncompatibility>;
}

#[derive(Debug, Error)]
pub enum CompatibilityError {
    #[error("specs have incompatible boards")]
    Board(#[from] BoardIncompatibility),

    #[error("specs have incompatible pvpanic settings")]
    Pvpanic(#[from] PvpanicIncompatibility),

    #[error("collection {0} incompatible")]
    Collection(String, #[source] CollectionIncompatibility),

    #[cfg(feature = "falcon")]
    #[error("can't migrate instances containing softnpu devices")]
    SoftNpu,
}

#[derive(Debug, Error)]
pub enum BoardIncompatibility {
    #[error("boards have different CPU counts (self: {0}, other: {1})")]
    CpuCount(u8, u8),

    #[error("boards have different memory sizes (self: {0}, other: {1})")]
    MemorySize(u64, u64),

    #[error("chipsets have different PCIe settings (self: {0}, other: {1})")]
    PcieEnabled(bool, bool),
}

#[derive(Debug, Error)]
pub enum DiskIncompatibility {
    #[error("disks have different device interfaces (self: {0}, other: {1})")]
    Interface(&'static str, &'static str),

    #[error("disks have different PCI paths (self: {0}, other: {1}")]
    PciPath(PciPath, PciPath),

    #[error("disks have different backend names (self: {0}, other: {1}")]
    BackendName(String, String),

    #[error("disks have different backend kinds (self: {0}, other: {1}")]
    BackendKind(&'static str, &'static str),

    #[error("disks have different read-only settings (self: {0}, other: {1}")]
    ReadOnly(bool, bool),
}

#[derive(Debug, Error)]
pub enum NicIncompatibility {
    #[error("NICs have different PCI paths (self: {0}, other: {1})")]
    PciPath(PciPath, PciPath),

    #[error("NICs have different backend names (self: {0}, other: {1}")]
    BackendName(String, String),
}

#[derive(Debug, Error)]
pub enum SerialPortIncompatibility {
    #[error("ports have different numbers (self: {0:?}, other: {1:?})")]
    Number(SerialPortNumber, SerialPortNumber),

    #[error("ports have different consumers (self: {0}, other: {1})")]
    User(SerialPortUser, SerialPortUser),
}

#[derive(Debug, Error)]
pub enum BridgeIncompatibility {
    #[error("bridges have different PCI paths (self: {0}, other: {1})")]
    PciPath(PciPath, PciPath),

    #[error("bridges have different downstream buses (self: {0}, other: {1})")]
    DownstreamBus(u8, u8),
}

#[derive(Debug, Error)]
pub enum PvpanicIncompatibility {
    #[error("pvpanic presence differs (self: {0}, other: {1})")]
    Presence(bool, bool),

    #[error("pvpanic devices have different names (self: {0}, other: {1})")]
    Name(String, String),

    #[error(
        "pvpanic devices have different ISA settings (self: {0}, other: {1})"
    )]
    EnableIsa(bool, bool),
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
    #[error("collections have different lengths (self: {0}, other: {1})")]
    Length(usize, usize),

    #[error("collection key {0} present in self but not other")]
    KeyAbsent(String),

    #[error("component {0} incompatible")]
    Component(String, #[source] ComponentIncompatibility),
}

impl<T: CompatComponent> CompatCollection for HashMap<String, T> {
    fn is_compatible_with(
        &self,
        other: &Self,
    ) -> Result<(), CollectionIncompatibility> {
        if self.len() != other.len() {
            return Err(CollectionIncompatibility::Length(
                self.len(),
                other.len(),
            ));
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

        let this = &self.board;
        let other = &other.board;
        if this.cpus != other.cpus {
            Err(BoardIncompatibility::CpuCount(this.cpus, other.cpus))
        } else if this.memory_mb != other.memory_mb {
            Err(BoardIncompatibility::MemorySize(
                this.memory_mb,
                other.memory_mb,
            ))
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
            Err(BoardIncompatibility::PcieEnabled(
                this.enable_pcie,
                other.enable_pcie,
            ))
        } else {
            Ok(())
        }
    }

    fn is_pvpanic_compatible(
        &self,
        other: &Self,
    ) -> Result<(), PvpanicIncompatibility> {
        match (&self.pvpanic, &other.pvpanic) {
            (Some(this), Some(other)) => {
                if this.name != other.name {
                    Err(PvpanicIncompatibility::Name(
                        this.name.clone(),
                        other.name.clone(),
                    ))
                } else if this.spec.enable_isa != other.spec.enable_isa {
                    Err(PvpanicIncompatibility::EnableIsa(
                        this.spec.enable_isa,
                        other.spec.enable_isa,
                    ))
                } else {
                    Ok(())
                }
            }
            (None, None) => Ok(()),
            (this, other) => Err(PvpanicIncompatibility::Presence(
                this.is_some(),
                other.is_some(),
            )),
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
        if !self.softnpu.is_empty() || !other.softnpu.is_empty() {
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
            Err(DiskIncompatibility::Interface(self.kind(), other.kind()))
        } else if self.pci_path() != other.pci_path() {
            Err(DiskIncompatibility::PciPath(self.pci_path(), other.pci_path()))
        } else if self.backend_name() != other.backend_name() {
            Err(DiskIncompatibility::BackendName(
                self.backend_name().to_owned(),
                other.backend_name().to_owned(),
            ))
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
            Err(DiskIncompatibility::BackendKind(self.kind(), other.kind()))
        } else if self.read_only() != other.read_only() {
            Err(DiskIncompatibility::ReadOnly(
                self.read_only(),
                other.read_only(),
            ))
        } else {
            Ok(())
        }
    }
}

impl CompatComponent for spec::Disk {
    fn is_compatible_with(
        &self,
        other: &Self,
    ) -> Result<(), ComponentIncompatibility> {
        self.device_spec.is_compatible_with(&other.device_spec)?;
        self.backend_spec.is_compatible_with(&other.backend_spec)?;
        Ok(())
    }
}

impl CompatComponent for spec::Nic {
    fn is_compatible_with(
        &self,
        other: &Self,
    ) -> Result<(), ComponentIncompatibility> {
        if self.device_spec.pci_path != other.device_spec.pci_path {
            Err(NicIncompatibility::PciPath(
                self.device_spec.pci_path,
                other.device_spec.pci_path,
            ))
        } else if self.device_spec.backend_name
            != other.device_spec.backend_name
        {
            Err(NicIncompatibility::BackendName(
                self.device_spec.backend_name.clone(),
                other.device_spec.backend_name.clone(),
            ))
        } else {
            Ok(())
        }
        .map_err(ComponentIncompatibility::Nic)
    }
}

impl CompatComponent for spec::SerialPort {
    fn is_compatible_with(
        &self,
        other: &Self,
    ) -> Result<(), ComponentIncompatibility> {
        if self.num != other.num {
            Err(SerialPortIncompatibility::Number(self.num, other.num))
        } else if std::mem::discriminant(&self.user)
            != std::mem::discriminant(&other.user)
        {
            Err(SerialPortIncompatibility::User(self.user, other.user))
        } else {
            Ok(())
        }
        .map_err(ComponentIncompatibility::SerialPort)
    }
}

impl CompatComponent for PciPciBridge {
    fn is_compatible_with(
        &self,
        other: &Self,
    ) -> Result<(), ComponentIncompatibility> {
        if self.pci_path != other.pci_path {
            Err(BridgeIncompatibility::PciPath(self.pci_path, other.pci_path))
        } else if self.downstream_bus != other.downstream_bus {
            Err(BridgeIncompatibility::DownstreamBus(
                self.downstream_bus,
                other.downstream_bus,
            ))
        } else {
            Ok(())
        }
        .map_err(ComponentIncompatibility::PciPciBridge)
    }
}
