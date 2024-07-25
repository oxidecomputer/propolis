// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A builder for instance specs.

use std::collections::BTreeSet;

use propolis_api_types::{
    instance_spec::{
        components::{
            board::{Board, Chipset, I440Fx},
            devices::SerialPortNumber,
        },
        v0::{NetworkDeviceV0, StorageDeviceV0},
        PciPath,
    },
    DiskRequest, InstanceProperties, NetworkInterfaceRequest,
};
use thiserror::Error;

#[cfg(feature = "falcon")]
use propolis_api_types::instance_spec::components::devices::{
    P9fs, SoftNpuP9, SoftNpuPciPort,
};

use crate::config;

use super::{
    api_request::{self, DeviceRequestError},
    config_toml::{ConfigTomlError, ParsedConfig},
    Disk, Nic, PciPciBridge, QemuPvpanic,
};

#[cfg(feature = "falcon")]
use super::{SoftNpu, SoftNpuPort};

/// Errors that can arise while building an instance spec from component parts.
#[derive(Debug, Error)]
pub(crate) enum SpecBuilderError {
    #[error("error parsing config TOML")]
    ConfigToml(#[from] ConfigTomlError),

    #[error("error parsing device in ensure request")]
    DeviceRequest(#[from] DeviceRequestError),

    #[error("A component with name {0} already exists")]
    ComponentNameInUse(String),

    #[error("A PCI device is already attached at {0:?}")]
    PciPathInUse(PciPath),

    #[error("Serial port {0:?} is already specified")]
    SerialPortInUse(SerialPortNumber),

    #[error("pvpanic device already specified")]
    PvpanicInUse,
}

#[derive(Debug, Default)]
pub(crate) struct SpecBuilder {
    spec: super::Spec,
    pci_paths: BTreeSet<PciPath>,
    component_names: BTreeSet<String>,
}

trait PciComponent {
    fn pci_path(&self) -> PciPath;
}

impl PciComponent for StorageDeviceV0 {
    fn pci_path(&self) -> PciPath {
        match self {
            StorageDeviceV0::VirtioDisk(disk) => disk.pci_path,
            StorageDeviceV0::NvmeDisk(disk) => disk.pci_path,
        }
    }
}

impl PciComponent for NetworkDeviceV0 {
    fn pci_path(&self) -> PciPath {
        match self {
            NetworkDeviceV0::VirtioNic(nic) => nic.pci_path,
        }
    }
}

impl SpecBuilder {
    pub fn new(properties: &InstanceProperties) -> Self {
        let board = Board {
            cpus: properties.vcpus,
            memory_mb: properties.memory,
            chipset: Chipset::I440Fx(I440Fx { enable_pcie: false }),
        };

        Self {
            spec: super::Spec { board, ..Default::default() },
            ..Default::default()
        }
    }

    pub(super) fn with_board(board: Board) -> Self {
        Self {
            spec: super::Spec { board, ..Default::default() },
            ..Default::default()
        }
    }

    /// Converts an HTTP API request to add a NIC to an instance into
    /// device/backend entries in the spec under construction.
    pub fn add_nic_from_request(
        &mut self,
        nic: &NetworkInterfaceRequest,
    ) -> Result<(), SpecBuilderError> {
        self.add_network_device(api_request::parse_nic_from_request(nic)?)?;
        Ok(())
    }

    /// Converts an HTTP API request to add a disk to an instance into
    /// device/backend entries in the spec under construction.
    pub fn add_disk_from_request(
        &mut self,
        disk: &DiskRequest,
    ) -> Result<(), SpecBuilderError> {
        self.add_storage_device(api_request::parse_disk_from_request(disk)?)?;
        Ok(())
    }

    /// Converts an HTTP API request to add a cloud-init disk to an instance
    /// into device/backend entries in the spec under construction.
    pub fn add_cloud_init_from_request(
        &mut self,
        base64: String,
    ) -> Result<(), SpecBuilderError> {
        self.add_storage_device(api_request::parse_cloud_init_from_request(
            base64,
        )?)?;

        Ok(())
    }

    /// Adds all the devices and backends specified in the supplied
    /// configuration TOML to the spec under construction.
    pub fn add_devices_from_config(
        &mut self,
        config: &config::Config,
    ) -> Result<(), SpecBuilderError> {
        let parsed = ParsedConfig::try_from(config)?;

        let Chipset::I440Fx(ref mut i440fx) = self.spec.board.chipset;
        i440fx.enable_pcie = parsed.enable_pcie;

        for disk in parsed.disks {
            self.add_storage_device(disk)?;
        }

        for nic in parsed.nics {
            self.add_network_device(nic)?;
        }

        for bridge in parsed.pci_bridges {
            self.add_pci_bridge(bridge)?;
        }

        #[cfg(feature = "falcon")]
        self.add_parsed_softnpu_devices(parsed.softnpu)?;

        Ok(())
    }

    #[cfg(feature = "falcon")]
    fn add_parsed_softnpu_devices(
        &mut self,
        devices: SoftNpu,
    ) -> Result<(), SpecBuilderError> {
        if let Some(pci_port) = devices.pci_port {
            self.set_softnpu_pci_port(pci_port)?;
        }

        for port in devices.ports {
            self.add_softnpu_port(port)?;
        }

        if let Some(p9) = devices.p9_device {
            self.set_softnpu_p9(p9)?;
        }

        if let Some(p9fs) = devices.p9fs {
            self.set_p9fs(p9fs)?;
        }

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
        disk: Disk,
    ) -> Result<&Self, SpecBuilderError> {
        if self.component_names.contains(&disk.device_name) {
            return Err(SpecBuilderError::ComponentNameInUse(disk.device_name));
        }

        if self.component_names.contains(&disk.backend_name) {
            return Err(SpecBuilderError::ComponentNameInUse(
                disk.backend_name,
            ));
        }

        self.register_pci_device(disk.device_spec.pci_path())?;
        self.component_names.insert(disk.device_name.clone());
        self.component_names.insert(disk.backend_name.clone());
        self.spec.disks.push(disk);
        Ok(self)
    }

    /// Adds a network device with an associated backend.
    pub(super) fn add_network_device(
        &mut self,
        nic: Nic,
    ) -> Result<&Self, SpecBuilderError> {
        if self.component_names.contains(&nic.device_name) {
            return Err(SpecBuilderError::ComponentNameInUse(nic.device_name));
        }

        if self.component_names.contains(&nic.backend_name) {
            return Err(SpecBuilderError::ComponentNameInUse(nic.backend_name));
        }

        self.register_pci_device(nic.device_spec.pci_path())?;
        self.component_names.insert(nic.device_name.clone());
        self.component_names.insert(nic.backend_name.clone());
        self.spec.nics.push(nic);
        Ok(self)
    }

    /// Adds a PCI-PCI bridge.
    pub fn add_pci_bridge(
        &mut self,
        bridge: PciPciBridge,
    ) -> Result<&Self, SpecBuilderError> {
        let name = bridge.name();
        if self.component_names.contains(&name) {
            return Err(SpecBuilderError::ComponentNameInUse(name));
        }

        self.register_pci_device(bridge.0.pci_path)?;
        self.spec.pci_pci_bridges.push(bridge);
        Ok(self)
    }

    /// Adds a serial port.
    pub fn add_serial_port(
        &mut self,
        port: SerialPortNumber,
    ) -> Result<&Self, SpecBuilderError> {
        let idx = match port {
            SerialPortNumber::Com1 => 0,
            SerialPortNumber::Com2 => 1,
            SerialPortNumber::Com3 => 2,
            SerialPortNumber::Com4 => 3,
        };

        if self.spec.serial[idx] != super::SerialPort::Disabled {
            Err(SpecBuilderError::SerialPortInUse(port))
        } else {
            self.spec.serial[idx] = super::SerialPort::Enabled;
            Ok(self)
        }
    }

    pub fn add_pvpanic_device(
        &mut self,
        pvpanic: QemuPvpanic,
    ) -> Result<&Self, SpecBuilderError> {
        if self.spec.pvpanic.is_some() {
            return Err(SpecBuilderError::PvpanicInUse);
        }

        self.spec.pvpanic = Some(pvpanic);
        Ok(self)
    }

    #[cfg(feature = "falcon")]
    pub fn set_softnpu_com4(&mut self) -> Result<&Self, SpecBuilderError> {
        if self.spec.serial[3] != super::SerialPort::Disabled {
            Err(SpecBuilderError::SerialPortInUse(SerialPortNumber::Com4))
        } else {
            self.spec.serial[3] = super::SerialPort::SoftNpu;
            Ok(self)
        }
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
        port: SoftNpuPort,
    ) -> Result<&Self, SpecBuilderError> {
        if self.component_names.contains(&port.name) {
            return Err(SpecBuilderError::ComponentNameInUse(port.name));
        }

        if self.component_names.contains(&port.backend_name) {
            return Err(SpecBuilderError::ComponentNameInUse(
                port.backend_name,
            ));
        }

        self.spec.softnpu.ports.push(port);
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
        InstanceMetadata, Slot, VolumeConstructionRequest,
    };
    use uuid::Uuid;

    use super::*;

    fn test_metadata() -> InstanceMetadata {
        InstanceMetadata {
            silo_id: uuid::uuid!("556a67f8-8b14-4659-bd9f-d8f85ecd36bf"),
            project_id: uuid::uuid!("75f60038-daeb-4a1d-916a-5fa5b7237299"),
            sled_id: uuid::uuid!("43a789ac-a0dd-4e1e-ac33-acdada142faa"),
            sled_serial: "some-gimlet".into(),
            sled_revision: 1,
            sled_model: "abcd".into(),
        }
    }

    fn test_builder() -> SpecBuilder {
        SpecBuilder::new(&InstanceProperties {
            id: Default::default(),
            name: Default::default(),
            description: Default::default(),
            metadata: test_metadata(),
            image_id: Default::default(),
            bootrom_id: Default::default(),
            memory: 512,
            vcpus: 4,
        })
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
        assert!(builder.add_serial_port(SerialPortNumber::Com1).is_ok());
        assert!(builder.add_serial_port(SerialPortNumber::Com2).is_ok());
        assert!(builder.add_serial_port(SerialPortNumber::Com3).is_ok());
        assert!(builder.add_serial_port(SerialPortNumber::Com4).is_ok());
        assert!(builder.add_serial_port(SerialPortNumber::Com1).is_err());
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
}
