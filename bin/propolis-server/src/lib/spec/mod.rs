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
mod builder;
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

    #[error("Device {0} requested missing backend {1}")]
    DeviceMissingBackend(String, String),

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

    fn add_network_device_from_config(
        &mut self,
        name: &str,
        device: &config::Device,
    ) -> Result<(), ServerSpecBuilderError> {
        self.builder.add_network_device(
            config_toml::parse_network_device_from_config(name, device)?,
        )?;

        Ok(())
    }

    fn add_pci_bridge_from_config(
        &mut self,
        bridge: &config::PciBridge,
    ) -> Result<(), ServerSpecBuilderError> {
        let parsed = config_toml::parse_pci_bridge_from_config(bridge)?;
        self.builder.add_pci_bridge(parsed.name, parsed.bridge)?;
        Ok(())
    }

    /// Adds all the devices and backends specified in the supplied
    /// configuration TOML to the spec under construction.
    pub fn add_devices_from_config(
        &mut self,
        config: &config::Config,
    ) -> Result<(), ServerSpecBuilderError> {
        for (device_name, device) in config.devices.iter() {
            let driver = device.driver.as_str();
            match driver {
                // If this is a storage device, parse its "block_dev" property
                // to get the name of its corresponding backend.
                "pci-virtio-block" | "pci-nvme" => {
                    let device_spec =
                        config_toml::parse_storage_device_from_config(
                            device_name,
                            device,
                        )?;

                    let backend_name = match &device_spec {
                        StorageDeviceV0::VirtioDisk(disk) => {
                            disk.backend_name.clone()
                        }
                        StorageDeviceV0::NvmeDisk(disk) => {
                            disk.backend_name.clone()
                        }
                    };

                    let backend_config = config
                        .block_devs
                        .get(&backend_name)
                        .ok_or_else(|| {
                        ServerSpecBuilderError::DeviceMissingBackend(
                            device_name.clone(),
                            backend_name.clone(),
                        )
                    })?;

                    let backend_spec =
                        config_toml::parse_storage_backend_from_config(
                            &backend_name,
                            backend_config,
                        )?;

                    self.builder.add_storage_device(ParsedStorageDevice {
                        device_name: device_name.clone(),
                        device_spec,
                        backend_name,
                        backend_spec,
                    })?;
                }
                "pci-virtio-viona" => {
                    self.add_network_device_from_config(device_name, device)?
                }
                #[cfg(feature = "falcon")]
                "softnpu-pci-port" => {
                    self.add_softnpu_pci_port_from_config(device_name, device)?
                }
                #[cfg(feature = "falcon")]
                "softnpu-port" => {
                    self.add_softnpu_device_from_config(device_name, device)?
                }
                #[cfg(feature = "falcon")]
                "softnpu-p9" => {
                    self.add_softnpu_p9_from_config(device_name, device)?
                }
                #[cfg(feature = "falcon")]
                "pci-virtio-9p" => {
                    self.add_p9fs_from_config(device_name, device)?
                }
                _ => {
                    return Err(ServerSpecBuilderError::ConfigToml(
                        ConfigTomlError::UnrecognizedDeviceType(
                            driver.to_owned(),
                        ),
                    ))
                }
            }
        }

        for bridge in config.pci_bridges.iter() {
            self.add_pci_bridge_from_config(bridge)?;
        }

        Ok(())
    }

    #[cfg(feature = "falcon")]
    fn add_softnpu_p9_from_config(
        &mut self,
        name: &str,
        device: &config::Device,
    ) -> Result<(), ServerSpecBuilderError> {
        self.builder.set_softnpu_p9(
            config_toml::parse_softnpu_p9_from_config(name, device)?,
        )?;

        Ok(())
    }

    #[cfg(feature = "falcon")]
    fn add_softnpu_pci_port_from_config(
        &mut self,
        name: &str,
        device: &config::Device,
    ) -> Result<(), ServerSpecBuilderError> {
        self.builder.set_softnpu_pci_port(
            config_toml::parse_softnpu_pci_port_from_config(name, device)?,
        )?;

        Ok(())
    }

    #[cfg(feature = "falcon")]
    fn add_softnpu_device_from_config(
        &mut self,
        name: &str,
        device: &config::Device,
    ) -> Result<(), ServerSpecBuilderError> {
        let port = config_toml::parse_softnpu_port_from_config(name, device)?;
        self.builder.add_softnpu_port(name.to_string(), port)?;
        Ok(())
    }

    #[cfg(feature = "falcon")]
    fn add_p9fs_from_config(
        &mut self,
        name: &str,
        device: &config::Device,
    ) -> Result<(), ServerSpecBuilderError> {
        self.builder
            .set_p9fs(config_toml::parse_p9fs_from_config(name, device)?)?;

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
