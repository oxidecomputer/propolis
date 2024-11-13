// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A builder for instance specs.

use std::collections::{BTreeSet, HashSet};

use cpuid_utils::CpuidMapConversionError;
use propolis_api_types::{
    instance_spec::{
        components::{
            board::{Board as InstanceSpecBoard, Chipset, I440Fx},
            devices::{PciPciBridge, SerialPortNumber},
        },
        PciPath,
    },
    DiskRequest, NetworkInterfaceRequest,
};
use thiserror::Error;

#[cfg(feature = "falcon")]
use propolis_api_types::instance_spec::components::devices::{
    P9fs, SoftNpuP9, SoftNpuPciPort,
};

use crate::spec::SerialPortDevice;

use super::{
    api_request::{self, DeviceRequestError},
    Board, BootOrderEntry, BootSettings, Disk, Nic, QemuPvpanic, SerialPort,
};

#[cfg(not(feature = "omicron-build"))]
use super::MigrationFailure;

#[cfg(feature = "falcon")]
use super::SoftNpuPort;

/// Errors that can arise while building an instance spec from component parts.
#[derive(Debug, Error)]
pub(crate) enum SpecBuilderError {
    #[error("error parsing device in ensure request")]
    DeviceRequest(#[from] DeviceRequestError),

    #[error("device {0} has the same name as its backend")]
    DeviceAndBackendNamesIdentical(String),

    #[error("a component with name {0} already exists")]
    ComponentNameInUse(String),

    #[error("a PCI device is already attached at {0:?}")]
    PciPathInUse(PciPath),

    #[error("serial port {0:?} is already specified")]
    SerialPortInUse(SerialPortNumber),

    #[error("pvpanic device already specified")]
    PvpanicInUse,

    #[cfg(not(feature = "omicron-build"))]
    #[error("migration failure injection already enabled")]
    MigrationFailureInjectionInUse,

    #[error("boot settings were already specified")]
    BootSettingsInUse,

    #[error("boot option {0} is not an attached device")]
    BootOptionMissing(String),

    #[error("instance spec's CPUID entries are invalid")]
    CpuidEntriesInvalid(#[from] cpuid_utils::CpuidMapConversionError),
}

#[derive(Debug, Default)]
pub(crate) struct SpecBuilder {
    spec: super::Spec,
    pci_paths: BTreeSet<PciPath>,
    serial_ports: HashSet<SerialPortNumber>,
    component_names: BTreeSet<String>,
}

impl SpecBuilder {
    pub fn new(cpus: u8, memory_mb: u64) -> Self {
        let board = Board {
            cpus,
            memory_mb,
            chipset: Chipset::I440Fx(I440Fx { enable_pcie: false }),
        };

        Self {
            spec: super::Spec { board, ..Default::default() },
            ..Default::default()
        }
    }

    pub(super) fn with_instance_spec_board(
        board: InstanceSpecBoard,
    ) -> Result<Self, SpecBuilderError> {
        Ok(Self {
            spec: super::Spec {
                board: Board {
                    cpus: board.cpus,
                    memory_mb: board.memory_mb,
                    chipset: board.chipset,
                },
                cpuid: board
                    .cpuid
                    .map(|cpuid| -> Result<_, CpuidMapConversionError> {
                        {
                            Ok(cpuid_utils::CpuidSet::from_map(
                                cpuid.entries.try_into()?,
                                cpuid.vendor,
                            )?)
                        }
                    })
                    .transpose()?,
                ..Default::default()
            },
            ..Default::default()
        })
    }

    /// Converts an HTTP API request to add a NIC to an instance into
    /// device/backend entries in the spec under construction.
    pub fn add_nic_from_request(
        &mut self,
        nic: &NetworkInterfaceRequest,
    ) -> Result<(), SpecBuilderError> {
        let parsed = api_request::parse_nic_from_request(nic)?;
        self.add_network_device(parsed.name, parsed.nic)?;
        Ok(())
    }

    /// Converts an HTTP API request to add a disk to an instance into
    /// device/backend entries in the spec under construction.
    pub fn add_disk_from_request(
        &mut self,
        disk: &DiskRequest,
    ) -> Result<(), SpecBuilderError> {
        let parsed = api_request::parse_disk_from_request(disk)?;
        self.add_storage_device(parsed.name, parsed.disk)?;
        Ok(())
    }

    /// Sets the spec's boot order to the list of disk devices specified in
    /// `boot_options`.
    ///
    /// All of the items in the supplied `boot_options` must already be present
    /// in the spec's disk map.
    pub fn add_boot_order(
        &mut self,
        component_name: String,
        boot_options: impl Iterator<Item = BootOrderEntry>,
    ) -> Result<(), SpecBuilderError> {
        if self.component_names.contains(&component_name) {
            return Err(SpecBuilderError::ComponentNameInUse(component_name));
        }

        if self.spec.boot_settings.is_some() {
            return Err(SpecBuilderError::BootSettingsInUse);
        }

        let mut order = vec![];
        for item in boot_options {
            if !self.spec.disks.contains_key(item.name.as_str()) {
                return Err(SpecBuilderError::BootOptionMissing(
                    item.name.clone(),
                ));
            }

            order.push(crate::spec::BootOrderEntry { name: item.name.clone() });
        }

        self.spec.boot_settings =
            Some(BootSettings { name: component_name, order });
        Ok(())
    }

    /// Converts an HTTP API request to add a cloud-init disk to an instance
    /// into device/backend entries in the spec under construction.
    pub fn add_cloud_init_from_request(
        &mut self,
        base64: String,
    ) -> Result<(), SpecBuilderError> {
        let parsed = api_request::parse_cloud_init_from_request(base64)?;
        self.add_storage_device(parsed.name, parsed.disk)?;
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
        disk_name: String,
        disk: Disk,
    ) -> Result<&Self, SpecBuilderError> {
        if disk_name == disk.device_spec.backend_name() {
            return Err(SpecBuilderError::DeviceAndBackendNamesIdentical(
                disk_name,
            ));
        }

        if self.component_names.contains(&disk_name) {
            return Err(SpecBuilderError::ComponentNameInUse(disk_name));
        }

        if self.component_names.contains(disk.device_spec.backend_name()) {
            return Err(SpecBuilderError::ComponentNameInUse(
                disk.device_spec.backend_name().to_owned(),
            ));
        }

        self.register_pci_device(disk.device_spec.pci_path())?;
        self.component_names.insert(disk_name.clone());
        self.component_names.insert(disk.device_spec.backend_name().to_owned());
        let _old = self.spec.disks.insert(disk_name, disk);
        assert!(_old.is_none());
        Ok(self)
    }

    /// Adds a network device with an associated backend.
    pub(super) fn add_network_device(
        &mut self,
        nic_name: String,
        nic: Nic,
    ) -> Result<&Self, SpecBuilderError> {
        if nic_name == nic.device_spec.backend_name {
            return Err(SpecBuilderError::DeviceAndBackendNamesIdentical(
                nic_name,
            ));
        }

        if self.component_names.contains(&nic_name) {
            return Err(SpecBuilderError::ComponentNameInUse(nic_name));
        }

        if self.component_names.contains(&nic.device_spec.backend_name) {
            return Err(SpecBuilderError::ComponentNameInUse(
                nic.device_spec.backend_name,
            ));
        }

        self.register_pci_device(nic.device_spec.pci_path)?;
        self.component_names.insert(nic_name.clone());
        self.component_names.insert(nic.device_spec.backend_name.clone());
        let _old = self.spec.nics.insert(nic_name, nic);
        assert!(_old.is_none());
        Ok(self)
    }

    /// Adds a PCI-PCI bridge.
    pub fn add_pci_bridge(
        &mut self,
        name: String,
        bridge: PciPciBridge,
    ) -> Result<&Self, SpecBuilderError> {
        if self.component_names.contains(&name) {
            return Err(SpecBuilderError::ComponentNameInUse(name));
        }

        self.register_pci_device(bridge.pci_path)?;
        self.component_names.insert(name.clone());
        let _old = self.spec.pci_pci_bridges.insert(name, bridge);
        assert!(_old.is_none());
        Ok(self)
    }

    /// Adds a serial port.
    pub fn add_serial_port(
        &mut self,
        name: String,
        num: SerialPortNumber,
    ) -> Result<&Self, SpecBuilderError> {
        if self.component_names.contains(&name) {
            return Err(SpecBuilderError::ComponentNameInUse(name));
        }

        if self.serial_ports.contains(&num) {
            return Err(SpecBuilderError::SerialPortInUse(num));
        }

        let desc = SerialPort { num, device: SerialPortDevice::Uart };
        self.spec.serial.insert(name.clone(), desc);
        self.component_names.insert(name);
        self.serial_ports.insert(num);
        Ok(self)
    }

    pub fn add_pvpanic_device(
        &mut self,
        pvpanic: QemuPvpanic,
    ) -> Result<&Self, SpecBuilderError> {
        if self.component_names.contains(&pvpanic.name) {
            return Err(SpecBuilderError::ComponentNameInUse(pvpanic.name));
        }

        if self.spec.pvpanic.is_some() {
            return Err(SpecBuilderError::PvpanicInUse);
        }

        self.component_names.insert(pvpanic.name.clone());
        self.spec.pvpanic = Some(pvpanic);
        Ok(self)
    }

    #[cfg(not(feature = "omicron-build"))]
    pub fn add_migration_failure_device(
        &mut self,
        mig: MigrationFailure,
    ) -> Result<&Self, SpecBuilderError> {
        if self.component_names.contains(&mig.name) {
            return Err(SpecBuilderError::ComponentNameInUse(mig.name));
        }

        if self.spec.migration_failure.is_some() {
            return Err(SpecBuilderError::MigrationFailureInjectionInUse);
        }

        self.component_names.insert(mig.name.clone());
        self.spec.migration_failure = Some(mig);
        Ok(self)
    }

    #[cfg(feature = "falcon")]
    pub fn set_softnpu_com4(
        &mut self,
        name: String,
    ) -> Result<&Self, SpecBuilderError> {
        if self.component_names.contains(&name) {
            return Err(SpecBuilderError::ComponentNameInUse(name));
        }

        let num = SerialPortNumber::Com4;
        if self.serial_ports.contains(&num) {
            return Err(SpecBuilderError::SerialPortInUse(num));
        }

        let desc = SerialPort { num, device: SerialPortDevice::SoftNpu };
        self.spec.serial.insert(name.clone(), desc);
        self.component_names.insert(name);
        self.serial_ports.insert(num);
        Ok(self)
    }

    #[cfg(feature = "falcon")]
    pub fn set_softnpu_pci_port(
        &mut self,
        pci_port: SoftNpuPciPort,
    ) -> Result<&Self, SpecBuilderError> {
        self.register_pci_device(pci_port.pci_path)?;
        self.spec.softnpu.pci_port = Some(pci_port);
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
        port_name: String,
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
    use propolis_api_types::{
        instance_spec::components::{
            backends::{BlobStorageBackend, VirtioNetworkBackend},
            devices::{VirtioDisk, VirtioNic},
        },
        Slot, VolumeConstructionRequest,
    };
    use uuid::Uuid;

    use crate::spec::{StorageBackend, StorageDevice};

    use super::*;

    fn test_builder() -> SpecBuilder {
        SpecBuilder::new(4, 512)
    }

    #[test]
    fn duplicate_pci_slot() {
        let mut builder = test_builder();
        // Adding the same disk device twice should fail.
        assert!(builder
            .add_disk_from_request(&DiskRequest {
                name: "disk1".to_string(),
                slot: Slot(0),
                read_only: true,
                device: "nvme".to_string(),
                volume_construction_request: VolumeConstructionRequest::File {
                    id: Uuid::new_v4(),
                    block_size: 512,
                    path: "disk1.img".to_string()
                },
            })
            .is_ok());

        assert!(builder
            .add_disk_from_request(&DiskRequest {
                name: "disk2".to_string(),
                slot: Slot(0),
                read_only: true,
                device: "virtio".to_string(),
                volume_construction_request: VolumeConstructionRequest::File {
                    id: Uuid::new_v4(),
                    block_size: 512,
                    path: "disk2.img".to_string()
                },
            })
            .is_err());
    }

    #[test]
    fn duplicate_serial_port() {
        let mut builder = test_builder();
        assert!(builder
            .add_serial_port("com1".to_owned(), SerialPortNumber::Com1)
            .is_ok());
        assert!(builder
            .add_serial_port("com2".to_owned(), SerialPortNumber::Com2)
            .is_ok());
        assert!(builder
            .add_serial_port("com3".to_owned(), SerialPortNumber::Com3)
            .is_ok());
        assert!(builder
            .add_serial_port("com4".to_owned(), SerialPortNumber::Com4)
            .is_ok());
        assert!(builder
            .add_serial_port("com1".to_owned(), SerialPortNumber::Com1)
            .is_err());
    }

    #[test]
    fn unknown_storage_device_type() {
        let mut builder = test_builder();
        assert!(builder
            .add_disk_from_request(&DiskRequest {
                name: "disk3".to_string(),
                slot: Slot(0),
                read_only: true,
                device: "virtio-scsi".to_string(),
                volume_construction_request: VolumeConstructionRequest::File {
                    id: Uuid::new_v4(),
                    block_size: 512,
                    path: "disk3.img".to_string()
                },
            })
            .is_err());
    }

    #[test]
    fn device_with_same_name_as_backend() {
        let mut builder = test_builder();
        assert!(builder
            .add_storage_device(
                "storage".to_owned(),
                Disk {
                    device_spec: StorageDevice::Virtio(VirtioDisk {
                        backend_name: "storage".to_owned(),
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
                "network".to_owned(),
                Nic {
                    device_spec: VirtioNic {
                        backend_name: "network".to_owned(),
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
