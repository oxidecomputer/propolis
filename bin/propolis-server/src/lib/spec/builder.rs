// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A builder for instance specs.

use std::collections::{BTreeSet, HashSet};

use propolis_api_types::instance_spec::{
    components::{
        board::Board as InstanceSpecBoard,
        devices::{PciPciBridge, SerialPortNumber},
    },
    PciPath, SpecKey,
};
use thiserror::Error;

#[cfg(feature = "falcon")]
use propolis_api_types::instance_spec::components::devices::{
    P9fs, SoftNpuP9, SoftNpuPciPort,
};

use crate::spec::SerialPortDevice;

use super::{
    Board, BootOrderEntry, BootSettings, Disk, Nic, QemuPvpanic, SerialPort,
};

#[cfg(feature = "failure-injection")]
use super::MigrationFailure;

#[cfg(feature = "falcon")]
use super::SoftNpuPort;

/// Errors that can arise while building an instance spec from component parts.
#[derive(Debug, Error)]
pub(crate) enum SpecBuilderError {
    #[error("device {0} has the same name as its backend")]
    DeviceAndBackendNamesIdentical(SpecKey),

    #[error("a component with name {0} already exists")]
    ComponentNameInUse(SpecKey),

    #[error("a PCI device is already attached at {0:?}")]
    PciPathInUse(PciPath),

    #[error("serial port {0:?} is already specified")]
    SerialPortInUse(SerialPortNumber),

    #[error("pvpanic device already specified")]
    PvpanicInUse,

    #[cfg(feature = "failure-injection")]
    #[error("migration failure injection already enabled")]
    MigrationFailureInjectionInUse,

    #[error("boot settings were already specified")]
    BootSettingsInUse,

    #[error("boot option {0} is not an attached device")]
    BootOptionMissing(SpecKey),

    #[error("instance spec's CPUID entries are invalid")]
    CpuidEntriesInvalid(#[from] cpuid_utils::CpuidMapConversionError),

    #[error("failed to read default CPUID settings from the host")]
    DefaultCpuidReadFailed(#[from] cpuid_utils::host::GetHostCpuidError),
}

#[derive(Debug, Default)]
pub(crate) struct SpecBuilder {
    spec: super::Spec,
    pci_paths: BTreeSet<PciPath>,
    serial_ports: HashSet<SerialPortNumber>,
    component_names: BTreeSet<SpecKey>,
}

impl SpecBuilder {
    pub(super) fn with_instance_spec_board(
        board: InstanceSpecBoard,
    ) -> Result<Self, SpecBuilderError> {
        let cpuid = match board.cpuid {
            Some(cpuid) => cpuid_utils::CpuidSet::from_map(
                cpuid.entries.try_into()?,
                cpuid.vendor,
            ),
            None => cpuid_utils::host::query_complete(
                cpuid_utils::host::CpuidSource::BhyveDefault,
            )?,
        };

        Ok(Self {
            spec: super::Spec {
                board: Board {
                    cpus: board.cpus,
                    memory_mb: board.memory_mb,
                    chipset: board.chipset,
                    guest_hv_interface: board.guest_hv_interface,
                    native_acpi_tables: board.native_acpi_tables,
                },
                cpuid,
                ..Default::default()
            },
            ..Default::default()
        })
    }

    /// Sets the spec's boot order to the list of disk devices specified in
    /// `boot_options`.
    ///
    /// All of the items in the supplied `boot_options` must already be present
    /// in the spec's disk map.
    pub fn add_boot_order(
        &mut self,
        component_id: SpecKey,
        boot_options: impl Iterator<Item = BootOrderEntry>,
    ) -> Result<(), SpecBuilderError> {
        if self.component_names.contains(&component_id) {
            return Err(SpecBuilderError::ComponentNameInUse(component_id));
        }

        if self.spec.boot_settings.is_some() {
            return Err(SpecBuilderError::BootSettingsInUse);
        }

        let mut order = vec![];
        for item in boot_options {
            if !self.spec.disks.contains_key(&item.device_id) {
                return Err(SpecBuilderError::BootOptionMissing(
                    item.device_id.clone(),
                ));
            }

            order.push(crate::spec::BootOrderEntry {
                device_id: item.device_id.clone(),
            });
        }

        self.spec.boot_settings =
            Some(BootSettings { name: component_id, order });
        Ok(())
    }

    /// Adds a PCI path to this builder's record of PCI locations with an
    /// attached device. If the path is already in use, returns an error.
    fn register_pci_device(
        &mut self,
        pci_path: PciPath,
    ) -> Result<(), SpecBuilderError> {
        if self.pci_paths.contains(&pci_path) {
            Err(SpecBuilderError::PciPathInUse(pci_path))
        } else {
            self.pci_paths.insert(pci_path);
            Ok(())
        }
    }

    /// Adds a storage device with an associated backend.
    pub(super) fn add_storage_device(
        &mut self,
        disk_id: SpecKey,
        disk: Disk,
    ) -> Result<&Self, SpecBuilderError> {
        if disk_id == *disk.device_spec.backend_id() {
            return Err(SpecBuilderError::DeviceAndBackendNamesIdentical(
                disk_id,
            ));
        }

        if self.component_names.contains(&disk_id) {
            return Err(SpecBuilderError::ComponentNameInUse(disk_id));
        }

        if self.component_names.contains(disk.device_spec.backend_id()) {
            return Err(SpecBuilderError::ComponentNameInUse(
                disk.device_spec.backend_id().to_owned(),
            ));
        }

        self.register_pci_device(disk.device_spec.pci_path())?;
        self.component_names.insert(disk_id.clone());
        self.component_names.insert(disk.device_spec.backend_id().to_owned());
        let _old = self.spec.disks.insert(disk_id, disk);
        assert!(_old.is_none());
        Ok(self)
    }

    /// Adds a network device with an associated backend.
    pub(super) fn add_network_device(
        &mut self,
        nic_id: SpecKey,
        nic: Nic,
    ) -> Result<&Self, SpecBuilderError> {
        if nic_id == nic.device_spec.backend_id {
            return Err(SpecBuilderError::DeviceAndBackendNamesIdentical(
                nic_id,
            ));
        }

        if self.component_names.contains(&nic_id) {
            return Err(SpecBuilderError::ComponentNameInUse(nic_id));
        }

        if self.component_names.contains(&nic.device_spec.backend_id) {
            return Err(SpecBuilderError::ComponentNameInUse(
                nic.device_spec.backend_id,
            ));
        }

        self.register_pci_device(nic.device_spec.pci_path)?;
        self.component_names.insert(nic_id.clone());
        self.component_names.insert(nic.device_spec.backend_id.clone());
        let _old = self.spec.nics.insert(nic_id, nic);
        assert!(_old.is_none());
        Ok(self)
    }

    /// Adds a PCI-PCI bridge.
    pub fn add_pci_bridge(
        &mut self,
        id: SpecKey,
        bridge: PciPciBridge,
    ) -> Result<&Self, SpecBuilderError> {
        if self.component_names.contains(&id) {
            return Err(SpecBuilderError::ComponentNameInUse(id));
        }

        self.register_pci_device(bridge.pci_path)?;
        self.component_names.insert(id.clone());
        let _old = self.spec.pci_pci_bridges.insert(id, bridge);
        assert!(_old.is_none());
        Ok(self)
    }

    /// Adds a serial port.
    pub fn add_serial_port(
        &mut self,
        id: SpecKey,
        num: SerialPortNumber,
    ) -> Result<&Self, SpecBuilderError> {
        if self.component_names.contains(&id) {
            return Err(SpecBuilderError::ComponentNameInUse(id));
        }

        if self.serial_ports.contains(&num) {
            return Err(SpecBuilderError::SerialPortInUse(num));
        }

        let desc = SerialPort { num, device: SerialPortDevice::Uart };
        self.spec.serial.insert(id.clone(), desc);
        self.component_names.insert(id);
        self.serial_ports.insert(num);
        Ok(self)
    }

    pub fn add_pvpanic_device(
        &mut self,
        pvpanic: QemuPvpanic,
    ) -> Result<&Self, SpecBuilderError> {
        if self.component_names.contains(&pvpanic.id) {
            return Err(SpecBuilderError::ComponentNameInUse(pvpanic.id));
        }

        if self.spec.pvpanic.is_some() {
            return Err(SpecBuilderError::PvpanicInUse);
        }

        self.component_names.insert(pvpanic.id.clone());
        self.spec.pvpanic = Some(pvpanic);
        Ok(self)
    }

    #[cfg(feature = "failure-injection")]
    pub fn add_migration_failure_device(
        &mut self,
        mig: MigrationFailure,
    ) -> Result<&Self, SpecBuilderError> {
        if self.component_names.contains(&mig.id) {
            return Err(SpecBuilderError::ComponentNameInUse(mig.id));
        }

        if self.spec.migration_failure.is_some() {
            return Err(SpecBuilderError::MigrationFailureInjectionInUse);
        }

        self.component_names.insert(mig.id.clone());
        self.spec.migration_failure = Some(mig);
        Ok(self)
    }

    #[cfg(feature = "falcon")]
    pub fn set_softnpu_pci_port(
        &mut self,
        pci_port: SoftNpuPciPort,
    ) -> Result<&Self, SpecBuilderError> {
        // SoftNPU squats on COM4.
        let id = SpecKey::Name("com4".to_string());
        let num = SerialPortNumber::Com4;
        if self.component_names.contains(&id) {
            return Err(SpecBuilderError::ComponentNameInUse(id));
        }

        if self.serial_ports.contains(&num) {
            return Err(SpecBuilderError::SerialPortInUse(num));
        }

        self.register_pci_device(pci_port.pci_path)?;
        self.spec.softnpu.pci_port = Some(pci_port);
        self.spec
            .serial
            .insert(id, SerialPort { num, device: SerialPortDevice::SoftNpu });
        Ok(self)
    }

    #[cfg(feature = "falcon")]
    pub fn set_softnpu_p9(
        &mut self,
        p9: SoftNpuP9,
    ) -> Result<&Self, SpecBuilderError> {
        self.register_pci_device(p9.pci_path)?;
        self.spec.softnpu.p9_device = Some(p9);
        Ok(self)
    }

    #[cfg(feature = "falcon")]
    pub fn set_p9fs(&mut self, p9fs: P9fs) -> Result<&Self, SpecBuilderError> {
        self.register_pci_device(p9fs.pci_path)?;
        self.spec.softnpu.p9fs = Some(p9fs);
        Ok(self)
    }

    #[cfg(feature = "falcon")]
    pub fn add_softnpu_port(
        &mut self,
        port_name: SpecKey,
        port: SoftNpuPort,
    ) -> Result<&Self, SpecBuilderError> {
        if port_name == port.backend_name {
            return Err(SpecBuilderError::DeviceAndBackendNamesIdentical(
                port_name,
            ));
        }

        if self.component_names.contains(&port_name) {
            return Err(SpecBuilderError::ComponentNameInUse(port_name));
        }

        if self.component_names.contains(&port.backend_name) {
            return Err(SpecBuilderError::ComponentNameInUse(
                port.backend_name,
            ));
        }

        let _old = self.spec.softnpu.ports.insert(port_name, port);
        assert!(_old.is_none());
        Ok(self)
    }

    /// Yields the completed spec, consuming the builder.
    pub fn finish(self) -> super::Spec {
        self.spec
    }
}

#[cfg(test)]
mod test {
    use propolis_api_types::instance_spec::components::{
        backends::{BlobStorageBackend, VirtioNetworkBackend},
        board::{Chipset, GuestHypervisorInterface, I440Fx},
        devices::{VirtioDisk, VirtioNic},
    };
    use uuid::Uuid;

    use crate::spec::{StorageBackend, StorageDevice};

    use super::*;

    fn test_builder() -> SpecBuilder {
        let board = Board {
            cpus: 4,
            memory_mb: 512,
            chipset: Chipset::I440Fx(I440Fx { enable_pcie: false }),
            guest_hv_interface: GuestHypervisorInterface::Bhyve,
            native_acpi_tables: Some(false),
        };

        SpecBuilder {
            spec: crate::spec::Spec { board, ..Default::default() },
            ..Default::default()
        }
    }

    #[test]
    fn duplicate_pci_slot() {
        let mut builder = test_builder();
        assert!(builder
            .add_storage_device(
                SpecKey::Name("storage".to_owned()),
                Disk {
                    device_spec: StorageDevice::Virtio(VirtioDisk {
                        backend_id: SpecKey::Name("storage-backend".to_owned()),
                        pci_path: PciPath::new(0, 4, 0).unwrap()
                    }),
                    backend_spec: StorageBackend::Blob(BlobStorageBackend {
                        base64: "".to_string(),
                        readonly: false
                    })
                }
            )
            .is_ok());

        assert!(builder
            .add_network_device(
                SpecKey::Name("network".to_owned()),
                Nic {
                    device_spec: VirtioNic {
                        backend_id: SpecKey::Name("network-backend".to_owned()),
                        interface_id: Uuid::nil(),
                        pci_path: PciPath::new(0, 4, 0).unwrap()
                    },
                    backend_spec: VirtioNetworkBackend {
                        vnic_name: "vnic0".to_owned()
                    }
                }
            )
            .is_err());
    }

    #[test]
    fn duplicate_serial_port() {
        let mut builder = test_builder();
        assert!(builder
            .add_serial_port(
                SpecKey::Name("com1".to_owned()),
                SerialPortNumber::Com1
            )
            .is_ok());
        assert!(builder
            .add_serial_port(
                SpecKey::Name("com2".to_owned()),
                SerialPortNumber::Com2
            )
            .is_ok());
        assert!(builder
            .add_serial_port(
                SpecKey::Name("com3".to_owned()),
                SerialPortNumber::Com3
            )
            .is_ok());
        assert!(builder
            .add_serial_port(
                SpecKey::Name("com4".to_owned()),
                SerialPortNumber::Com4
            )
            .is_ok());
        assert!(builder
            .add_serial_port(
                SpecKey::Name("com1".to_owned()),
                SerialPortNumber::Com1
            )
            .is_err());
    }

    #[test]
    fn device_with_same_name_as_backend() {
        let mut builder = test_builder();
        assert!(builder
            .add_storage_device(
                SpecKey::Name("storage".to_owned()),
                Disk {
                    device_spec: StorageDevice::Virtio(VirtioDisk {
                        backend_id: SpecKey::Name("storage".to_owned()),
                        pci_path: PciPath::new(0, 4, 0).unwrap()
                    }),
                    backend_spec: StorageBackend::Blob(BlobStorageBackend {
                        base64: "".to_string(),
                        readonly: false
                    })
                }
            )
            .is_err());

        assert!(builder
            .add_network_device(
                SpecKey::Name("network".to_owned()),
                Nic {
                    device_spec: VirtioNic {
                        backend_id: SpecKey::Name("network".to_owned()),
                        interface_id: Uuid::nil(),
                        pci_path: PciPath::new(0, 5, 0).unwrap()
                    },
                    backend_spec: VirtioNetworkBackend {
                        vnic_name: "vnic0".to_owned()
                    }
                }
            )
            .is_err());
    }
}
