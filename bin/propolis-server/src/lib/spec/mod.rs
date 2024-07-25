// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Helper functions for building instance specs from server parameters.

use crate::config;
use api_request::DeviceRequestError;
use builder::SpecBuilder;
use config_toml::ConfigTomlError;
use propolis_api_types::instance_spec::components::board::{Chipset, I440Fx};
use propolis_api_types::instance_spec::components::devices::{
    QemuPvpanic, SerialPortNumber,
};
use propolis_api_types::instance_spec::{
    components::board::Board, v0::*, PciPath,
};
use propolis_api_types::{
    DiskRequest, InstanceProperties, NetworkInterfaceRequest,
};
use thiserror::Error;

mod api_request;
pub(crate) mod builder;
mod config_toml;

/// Describes a storage device/backend pair parsed from an input source like an
/// API request or a config TOMl entry.
struct ParsedStorageDevice {
    device_name: String,
    device_spec: StorageDeviceV0,
    backend_name: String,
    backend_spec: StorageBackendV0,
}

/// Describes a network device/backend pair parsed from an input source like an
/// API request or a config TOMl entry.
struct ParsedNetworkDevice {
    device_name: String,
    device_spec: NetworkDeviceV0,
    backend_name: String,
    backend_spec: NetworkBackendV0,
}

/// Errors that can occur while building an instance spec from component parts.
#[derive(Debug, Error)]
pub(crate) enum ServerSpecBuilderError {
    #[error(transparent)]
    InnerBuilderError(#[from] builder::SpecBuilderError),

    #[error("error parsing config TOML")]
    ConfigToml(#[from] ConfigTomlError),

    #[error("error parsing device in ensure request")]
    DeviceRequest(#[from] DeviceRequestError),
}

/// Generates NIC device and backend names from the NIC's PCI path. This is
/// needed because the `name` field in a propolis-client
/// `NetworkInterfaceRequest` is actually the name of the host vNIC to bind to,
/// and that can change between incarnations of an instance. The PCI path is
/// unique to each NIC but must remain stable over a migration, so it's suitable
/// for use in this naming scheme.
///
/// N.B. Migrating a NIC requires the source and target to agree on these names,
///      so changing this routine's behavior will prevent Propolis processes
///      with the old behavior from migrating processes with the new behavior.
fn pci_path_to_nic_names(path: PciPath) -> (String, String) {
    (format!("vnic-{}", path), format!("vnic-{}-backend", path))
}

/// A helper for building instance specs out of component parts.
pub struct ServerSpecBuilder {
    builder: SpecBuilder,
}

impl ServerSpecBuilder {
    /// Creates a new spec builder from an instance's properties (supplied via
    /// the instance APIs) and the config TOML supplied at server startup.
    pub fn new(
        properties: &InstanceProperties,
        config: &config::Config,
    ) -> Result<Self, ServerSpecBuilderError> {
        let enable_pcie =
            config.chipset.options.get("enable-pcie").map_or_else(
                || Ok(false),
                |v| {
                    v.as_bool().ok_or_else(|| {
                        ServerSpecBuilderError::ConfigToml(
                            ConfigTomlError::EnablePcieParseFailed(
                                v.to_string(),
                            ),
                        )
                    })
                },
            )?;

        let mut builder = SpecBuilder::new(Board {
            cpus: properties.vcpus,
            memory_mb: properties.memory,
            chipset: Chipset::I440Fx(I440Fx { enable_pcie }),
        });

        builder.add_pvpanic_device(QemuPvpanic { enable_isa: true })?;

        Ok(Self { builder })
    }

    /// Converts an HTTP API request to add a NIC to an instance into
    /// device/backend entries in the spec under construction.
    pub fn add_nic_from_request(
        &mut self,
        nic: &NetworkInterfaceRequest,
    ) -> Result<(), ServerSpecBuilderError> {
        self.builder
            .add_network_device(api_request::parse_nic_from_request(nic)?)?;

        Ok(())
    }

    /// Converts an HTTP API request to add a disk to an instance into
    /// device/backend entries in the spec under construction.
    pub fn add_disk_from_request(
        &mut self,
        disk: &DiskRequest,
    ) -> Result<(), ServerSpecBuilderError> {
        self.builder
            .add_storage_device(api_request::parse_disk_from_request(disk)?)?;

        Ok(())
    }

    /// Converts an HTTP API request to add a cloud-init disk to an instance
    /// into device/backend entries in the spec under construction.
    pub fn add_cloud_init_from_request(
        &mut self,
        base64: String,
    ) -> Result<(), ServerSpecBuilderError> {
        self.builder.add_storage_device(
            api_request::parse_cloud_init_from_request(base64)?,
        )?;

        Ok(())
    }

    /// Adds all the devices and backends specified in the supplied
    /// configuration TOML to the spec under construction.
    pub fn add_devices_from_config(
        &mut self,
        config: &config::Config,
    ) -> Result<(), ServerSpecBuilderError> {
        let parsed = config_toml::ParsedConfig::try_from(config)?;
        for disk in parsed.disks {
            self.builder.add_storage_device(disk)?;
        }

        for nic in parsed.nics {
            self.builder.add_network_device(nic)?;
        }

        for bridge in parsed.pci_bridges {
            self.builder.add_pci_bridge(bridge.name, bridge.bridge)?;
        }

        #[cfg(feature = "falcon")]
        self.add_parsed_softnpu_devices(parsed.softnpu)?;

        Ok(())
    }

    #[cfg(feature = "falcon")]
    fn add_parsed_softnpu_devices(
        &mut self,
        devices: config_toml::ParsedSoftNpu,
    ) -> Result<(), ServerSpecBuilderError> {
        for pci_port in devices.pci_ports {
            self.builder.set_softnpu_pci_port(pci_port)?;
        }

        for port in devices.ports {
            self.builder.add_softnpu_port(port.name.clone(), port)?;
        }

        for p9 in devices.p9_devices {
            self.builder.set_softnpu_p9(p9)?;
        }

        for p9fs in devices.p9fs {
            self.builder.set_p9fs(p9fs)?;
        }

        Ok(())
    }

    /// Adds a serial port specification to the spec under construction.
    pub fn add_serial_port(
        &mut self,
        port: SerialPortNumber,
    ) -> Result<(), ServerSpecBuilderError> {
        self.builder.add_serial_port(port)?;
        Ok(())
    }

    pub fn finish(self) -> InstanceSpecV0 {
        self.builder.finish()
    }
}

#[cfg(test)]
mod test {
    use crucible_client_types::VolumeConstructionRequest;
    use propolis_api_types::{InstanceMetadata, Slot};
    use uuid::Uuid;

    use crate::config::Config;

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

    fn default_spec_builder(
    ) -> Result<ServerSpecBuilder, ServerSpecBuilderError> {
        ServerSpecBuilder::new(
            &InstanceProperties {
                id: Default::default(),
                name: Default::default(),
                description: Default::default(),
                metadata: test_metadata(),
                image_id: Default::default(),
                bootrom_id: Default::default(),
                memory: 512,
                vcpus: 4,
            },
            &Config::default(),
        )
    }

    #[test]
    fn make_default_builder() {
        assert!(default_spec_builder().is_ok());
    }

    #[test]
    fn duplicate_pci_slot() {
        let mut builder = default_spec_builder().unwrap();

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
        assert!(matches!(
            builder
                .add_disk_from_request(&DiskRequest {
                    name: "disk2".to_string(),
                    slot: Slot(0),
                    read_only: true,
                    device: "virtio".to_string(),
                    volume_construction_request:
                        VolumeConstructionRequest::File {
                            id: Uuid::new_v4(),
                            block_size: 512,
                            path: "disk2.img".to_string()
                        },
                })
                .err(),
            Some(ServerSpecBuilderError::InnerBuilderError(
                builder::SpecBuilderError::PciPathInUse(_)
            ))
        ));
    }

    #[test]
    fn duplicate_serial_port() {
        let mut builder = default_spec_builder().unwrap();
        assert!(builder.add_serial_port(SerialPortNumber::Com1).is_ok());
        assert!(builder.add_serial_port(SerialPortNumber::Com2).is_ok());
        assert!(builder.add_serial_port(SerialPortNumber::Com3).is_ok());
        assert!(builder.add_serial_port(SerialPortNumber::Com4).is_ok());
        assert!(matches!(
            builder.add_serial_port(SerialPortNumber::Com1).err(),
            Some(ServerSpecBuilderError::InnerBuilderError(
                builder::SpecBuilderError::SerialPortInUse(_)
            ))
        ));
    }

    #[test]
    fn unknown_storage_device_type() {
        let mut builder = default_spec_builder().unwrap();
        assert!(matches!(
            builder
                .add_disk_from_request(&DiskRequest {
                    name: "disk3".to_string(),
                    slot: Slot(0),
                    read_only: true,
                    device: "virtio-scsi".to_string(),
                    volume_construction_request:
                        VolumeConstructionRequest::File {
                            id: Uuid::new_v4(),
                            block_size: 512,
                            path: "disk3.img".to_string()
                        },
                })
                .err(),
            Some(ServerSpecBuilderError::DeviceRequest(_))
        ));
    }
}
