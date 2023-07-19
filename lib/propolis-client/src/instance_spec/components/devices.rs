//! Device configuration data: components that define VM properties that are
//! visible to a VM's guest software.

use crate::instance_spec::{migration::MigrationElement, PciPath};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// A disk that presents a virtio-block interface to the guest.
#[derive(Clone, Deserialize, Serialize, Debug, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct VirtioDisk {
    /// The name of the disk's backend component.
    pub backend_name: String,

    /// The PCI bus/device/function at which this disk should be attached.
    pub pci_path: PciPath,
}

impl MigrationElement for VirtioDisk {
    fn kind(&self) -> &'static str {
        "VirtioDisk"
    }

    fn can_migrate_from_element(
        &self,
        other: &Self,
    ) -> Result<(), crate::instance_spec::migration::ElementCompatibilityError>
    {
        if self.backend_name != other.backend_name {
            Err(MigrationCompatibilityError::BackendNameMismatch(
                self.backend_name.clone(),
                other.backend_name.clone(),
            )
            .into())
        } else if self.pci_path != other.pci_path {
            Err(MigrationCompatibilityError::PciPath(
                self.pci_path,
                other.pci_path,
            )
            .into())
        } else {
            Ok(())
        }
    }
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

impl MigrationElement for NvmeDisk {
    fn kind(&self) -> &'static str {
        "NvmeDisk"
    }

    fn can_migrate_from_element(
        &self,
        other: &Self,
    ) -> Result<(), crate::instance_spec::migration::ElementCompatibilityError>
    {
        if self.backend_name != other.backend_name {
            Err(MigrationCompatibilityError::BackendNameMismatch(
                self.backend_name.clone(),
                other.backend_name.clone(),
            )
            .into())
        } else if self.pci_path != other.pci_path {
            Err(MigrationCompatibilityError::PciPath(
                self.pci_path,
                other.pci_path,
            )
            .into())
        } else {
            Ok(())
        }
    }
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

impl MigrationElement for VirtioNic {
    fn kind(&self) -> &'static str {
        "VirtioNic"
    }

    fn can_migrate_from_element(
        &self,
        other: &Self,
    ) -> Result<(), crate::instance_spec::migration::ElementCompatibilityError>
    {
        if self.backend_name != other.backend_name {
            Err(MigrationCompatibilityError::BackendNameMismatch(
                self.backend_name.clone(),
                other.backend_name.clone(),
            )
            .into())
        } else if self.pci_path != other.pci_path {
            Err(MigrationCompatibilityError::PciPath(
                self.pci_path,
                other.pci_path,
            )
            .into())
        } else {
            Ok(())
        }
    }
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

impl MigrationElement for SerialPort {
    fn kind(&self) -> &'static str {
        "SerialPort"
    }

    fn can_migrate_from_element(
        &self,
        other: &Self,
    ) -> Result<(), crate::instance_spec::migration::ElementCompatibilityError>
    {
        if self != other {
            Err(MigrationCompatibilityError::ComponentConfiguration(format!(
                "serial port number mismatch (self: {0:?}, other: {1:?})",
                self, other
            ))
            .into())
        } else {
            Ok(())
        }
    }
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

impl MigrationElement for PciPciBridge {
    fn kind(&self) -> &'static str {
        "PciPciBridge"
    }

    fn can_migrate_from_element(
        &self,
        other: &Self,
    ) -> Result<(), crate::instance_spec::migration::ElementCompatibilityError>
    {
        if self.downstream_bus != other.downstream_bus {
            Err(MigrationCompatibilityError::ComponentConfiguration(format!(
                "bridge downstream bus mismatch (self: {0}, other: {1})",
                self.downstream_bus, other.downstream_bus
            ))
            .into())
        } else if self.pci_path != other.pci_path {
            Err(MigrationCompatibilityError::PciPath(
                self.pci_path,
                other.pci_path,
            )
            .into())
        } else {
            Ok(())
        }
    }
}

#[derive(Debug, Error)]
pub enum MigrationCompatibilityError {
    /// The two devices have mismatched backend names. This means that migration
    /// might fail to copy the source device's backend's state to the
    /// corresponding target backend.
    #[error("devices have different backend names (self: {0}, other: {1})")]
    BackendNameMismatch(String, String),

    #[error("PCI devices have different paths (self: {0:?}, other: {1:?})")]
    PciPath(PciPath, PciPath),

    #[error("component configurations incompatible: {0}")]
    ComponentConfiguration(String),
}

//
// Structs for Falcon devices. These devices don't support live migration.
//

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

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn compatible_virtio_disk() {
        let d1 = VirtioDisk {
            backend_name: "storage_backend".to_string(),
            pci_path: PciPath::new(0, 5, 0).unwrap(),
        };
        assert!(d1.can_migrate_from_element(&d1).is_ok());
    }

    #[test]
    fn incompatible_virtio_disk() {
        let d1 = VirtioDisk {
            backend_name: "storage_backend".to_string(),
            pci_path: PciPath::new(0, 5, 0).unwrap(),
        };

        let d2 = VirtioDisk { backend_name: "other_backend".to_string(), ..d1 };
        assert!(d1.can_migrate_from_element(&d2).is_err());

        let d2 = VirtioDisk {
            pci_path: PciPath::new(0, 6, 0).unwrap(),
            ..d1.clone()
        };
        assert!(d1.can_migrate_from_element(&d2).is_err());
    }

    #[test]
    fn compatible_nvme_disk() {
        let d1 = NvmeDisk {
            backend_name: "storage_backend".to_string(),
            pci_path: PciPath::new(0, 5, 0).unwrap(),
        };
        assert!(d1.can_migrate_from_element(&d1).is_ok());
    }

    #[test]
    fn incompatible_nvme_disk() {
        let d1 = NvmeDisk {
            backend_name: "storage_backend".to_string(),
            pci_path: PciPath::new(0, 5, 0).unwrap(),
        };

        let d2 = NvmeDisk { backend_name: "other_backend".to_string(), ..d1 };
        assert!(d1.can_migrate_from_element(&d2).is_err());

        let d2 =
            NvmeDisk { pci_path: PciPath::new(0, 6, 0).unwrap(), ..d1.clone() };
        assert!(d1.can_migrate_from_element(&d2).is_err());
    }

    #[test]
    fn compatible_virtio_nic() {
        let d1 = VirtioNic {
            backend_name: "storage_backend".to_string(),
            pci_path: PciPath::new(0, 5, 0).unwrap(),
        };
        assert!(d1.can_migrate_from_element(&d1).is_ok());
    }

    #[test]
    fn incompatible_virtio_nic() {
        let d1 = VirtioNic {
            backend_name: "storage_backend".to_string(),
            pci_path: PciPath::new(0, 5, 0).unwrap(),
        };

        let d2 = VirtioNic { backend_name: "other_backend".to_string(), ..d1 };
        assert!(d1.can_migrate_from_element(&d2).is_err());

        let d2 = VirtioNic {
            pci_path: PciPath::new(0, 6, 0).unwrap(),
            ..d1.clone()
        };
        assert!(d1.can_migrate_from_element(&d2).is_err());
    }

    #[test]
    fn serial_port_compatibility() {
        let ports = [
            SerialPortNumber::Com1,
            SerialPortNumber::Com2,
            SerialPortNumber::Com3,
            SerialPortNumber::Com4,
        ];

        for (p1, p2) in
            ports.into_iter().flat_map(|p| std::iter::repeat(p).zip(ports))
        {
            let can_migrate = SerialPort { num: p1 }
                .can_migrate_from_element(&SerialPort { num: p2 });

            assert_eq!(
                p1 == p2,
                can_migrate.is_ok(),
                "p1: {:?}, p2: {:?}",
                p1,
                p2
            );
        }
    }

    #[test]
    fn pci_bridge_compatibility() {
        let b1 = PciPciBridge {
            downstream_bus: 1,
            pci_path: PciPath::new(1, 2, 3).unwrap(),
        };

        let mut b2 = b1;
        assert!(b1.can_migrate_from_element(&b2).is_ok());

        b2.downstream_bus += 1;
        assert!(b1.can_migrate_from_element(&b2).is_err());
        b2.downstream_bus = b1.downstream_bus;

        b2.pci_path = PciPath::new(4, 5, 6).unwrap();
        assert!(b1.can_migrate_from_element(&b2).is_err());
    }
}
