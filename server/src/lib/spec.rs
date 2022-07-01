//! Helper functions for building instance specs from server parameters.

use std::collections::BTreeSet;
use std::convert::TryInto;
use std::str::FromStr;

use propolis_client::api::{
    self, DiskRequest, InstanceProperties, NetworkInterfaceRequest,
};
use propolis_client::instance_spec::*;

use thiserror::Error;

use crate::config;

/// Errors that can occur while building an instance spec from component parts.
#[derive(Debug, Error)]
pub enum SpecBuilderError {
    #[error("A device with name {0} already exists")]
    DeviceNameInUse(String),

    #[error("A backend with name {0} already exists")]
    BackendNameInUse(String),

    #[error("The opaque description of backend {0} was not serializable: {1}")]
    BackendSpecNotSerializable(String, serde_json::error::Error),

    #[error("The supplied cloud init bytes failed to decode: {0}")]
    CloudInitDecodeFailed(String),

    #[error("A PCI device is already attached at {0:?}")]
    PciPathInUse(PciPath),

    #[error("The string {0} could not be converted to a PCI path")]
    PciPathNotParseable(String),

    #[error("Serial port {0:?} is already specified")]
    SerialPortInUse(SerialPortNumber),

    #[error(
        "Could not translate PCI slot {0} for device type {1:?} to a PCI path"
    )]
    PciSlotInvalid(u8, SlotType),

    #[error("Unrecognized storage device interface {0}")]
    UnrecognizedStorageDevice(String),

    #[error("Unrecognized storage backend type {0}")]
    UnrecognizedStorageBackend(String),

    #[error("Error in server config TOML: {0}")]
    ConfigTomlError(String),
}

/// A type of PCI device. Device numbers on the PCI bus are partitioned by slot
/// type. If a client asks to attach a device of type X to PCI slot Y, the
/// server will assign the Yth device number in X's partition. The partitioning
/// scheme is defined by the implementation of the `slot_to_pci_path` utility
/// function.
#[derive(Clone, Copy, Debug)]
pub enum SlotType {
    NIC,
    Disk,
    CloudInit,
}

/// Translates a device type and PCI slot (as presented in an instance creation
/// request) into a concrete PCI path. See the documentation for [`SlotType`].
fn slot_to_pci_path(
    slot: api::Slot,
    ty: SlotType,
) -> Result<PciPath, SpecBuilderError> {
    match ty {
        // Slots for NICS: 0x08 -> 0x0F
        SlotType::NIC if slot.0 <= 7 => Ok(PciPath(0, slot.0 + 0x8, 0)),
        // Slots for Disks: 0x10 -> 0x17
        SlotType::Disk if slot.0 <= 7 => Ok(PciPath(0, slot.0 + 0x10, 0)),
        // Slot for CloudInit
        SlotType::CloudInit if slot.0 == 0 => Ok(PciPath(0, slot.0 + 0x18, 0)),
        _ => Err(SpecBuilderError::PciSlotInvalid(slot.0, ty)),
    }
}

/// A helper for building instance specs out of component parts.
pub struct SpecBuilder {
    spec: InstanceSpec,
    pci_paths: BTreeSet<PciPath>,
}

impl SpecBuilder {
    /// Creates a new spec builder from an instance's properties (supplied via
    /// the instance APIs) and the config TOML supplied at server startup.
    pub fn new(
        properties: &InstanceProperties,
        config: &config::Config,
    ) -> Result<Self, SpecBuilderError> {
        let enable_pcie =
            config.get_chipset().options.get("enable-pcie").map_or_else(
                || Ok(false),
                |v| {
                    v.as_bool().ok_or_else(|| {
                        SpecBuilderError::ConfigTomlError(format!(
                            "Invalid value {} for enable-pcie flag in chipset",
                            v
                        ))
                    })
                },
            )?;

        let board = Board {
            cpus: properties.vcpus,
            memory_mb: properties.memory,
            chipset: Chipset::I440Fx { enable_pcie },
        };

        Ok(Self {
            spec: InstanceSpec {
                board,
                storage_devices: Default::default(),
                storage_backends: Default::default(),
                network_devices: Default::default(),
                network_backends: Default::default(),
                serial_ports: Default::default(),
                pci_pci_bridges: Default::default(),
            },

            pci_paths: Default::default(),
        })
    }

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

    /// Converts an HTTP API request to add a NIC to an instance into
    /// device/backend entries in the spec under construction.
    pub fn add_nic_from_request(
        &mut self,
        nic: &NetworkInterfaceRequest,
    ) -> Result<(), SpecBuilderError> {
        let pci_path = slot_to_pci_path(nic.slot, SlotType::NIC)?;
        self.register_pci_device(pci_path)?;

        if self
            .spec
            .network_backends
            .insert(
                nic.name.to_string(),
                NetworkBackend { vnic_name: nic.name.to_string() },
            )
            .is_some()
        {
            return Err(SpecBuilderError::BackendNameInUse(
                nic.name.to_string(),
            ));
        }

        if self
            .spec
            .network_devices
            .insert(
                nic.name.to_string(),
                NetworkDevice { backend_name: nic.name.to_string(), pci_path },
            )
            .is_some()
        {
            return Err(SpecBuilderError::DeviceNameInUse(
                nic.name.to_string(),
            ));
        }

        Ok(())
    }

    /// Converts an HTTP API request to add a disk to an instance into
    /// device/backend entries in the spec under construction.
    pub fn add_disk_from_request(
        &mut self,
        disk: &DiskRequest,
    ) -> Result<(), SpecBuilderError> {
        let pci_path = slot_to_pci_path(disk.slot, SlotType::Disk)?;
        self.register_pci_device(pci_path)?;

        if self
            .spec
            .storage_backends
            .insert(
                disk.name.to_string(),
                StorageBackend {
                    kind: StorageBackendKind::Crucible {
                        gen: disk.gen,
                        req: (&disk.volume_construction_request)
                            .try_into()
                            .map_err(|e| {
                                SpecBuilderError::BackendSpecNotSerializable(
                                    disk.name.to_string(),
                                    e,
                                )
                            })?,
                    },
                    readonly: disk.read_only,
                },
            )
            .is_some()
        {
            return Err(SpecBuilderError::BackendNameInUse(
                disk.name.to_string(),
            ));
        }

        if self
            .spec
            .storage_devices
            .insert(
                disk.name.to_string(),
                StorageDevice {
                    kind: match disk.device.as_ref() {
                        "virtio" => StorageDeviceKind::Virtio,
                        "nvme" => StorageDeviceKind::Nvme,
                        _ => {
                            return Err(
                                SpecBuilderError::UnrecognizedStorageDevice(
                                    disk.device.clone(),
                                ),
                            );
                        }
                    },
                    backend_name: disk.name.to_string(),
                    pci_path,
                },
            )
            .is_some()
        {
            return Err(SpecBuilderError::DeviceNameInUse(
                disk.name.to_string(),
            ));
        }

        Ok(())
    }

    /// Converts an HTTP API request to add a cloud-init disk to an instance
    /// into device/backend entries in the spec under construction.
    pub fn add_cloud_init_from_request(
        &mut self,
        cloud_init_bytes: &str,
    ) -> Result<(), SpecBuilderError> {
        let name = "cloud-init";
        let pci_path = slot_to_pci_path(api::Slot(0), SlotType::CloudInit)?;
        self.register_pci_device(pci_path)?;
        let bytes = base64::decode(&cloud_init_bytes).map_err(|e| {
            SpecBuilderError::CloudInitDecodeFailed(e.to_string())
        })?;

        if self
            .spec
            .storage_backends
            .insert(
                name.to_string(),
                StorageBackend {
                    kind: StorageBackendKind::InMemory { bytes },
                    readonly: true,
                },
            )
            .is_some()
        {
            return Err(SpecBuilderError::BackendNameInUse(name.to_string()));
        }

        if self
            .spec
            .storage_devices
            .insert(
                name.to_string(),
                StorageDevice {
                    kind: StorageDeviceKind::Virtio,
                    backend_name: name.to_string(),
                    pci_path,
                },
            )
            .is_some()
        {
            return Err(SpecBuilderError::DeviceNameInUse(name.to_string()));
        }

        Ok(())
    }

    fn add_storage_backend_from_config(
        &mut self,
        name: &str,
        backend: &config::BlockDevice,
    ) -> Result<(), SpecBuilderError> {
        let backend_spec = StorageBackend {
            kind: match backend.bdtype.as_str() {
                "file" => StorageBackendKind::File {
                    path: backend
                        .options
                        .get("path")
                        .ok_or_else(|| {
                            SpecBuilderError::ConfigTomlError(format!(
                                "Invalid path for file backend {}",
                                name
                            ))
                        })?
                        .as_str()
                        .ok_or_else(|| {
                            SpecBuilderError::ConfigTomlError(format!(
                                "Couldn't parse path for file backend {}",
                                name
                            ))
                        })?
                        .to_string(),
                },
                _ => {
                    return Err(SpecBuilderError::UnrecognizedStorageBackend(
                        backend.bdtype.to_string(),
                    ))
                }
            },
            readonly: || -> Option<bool> {
                match backend.options.get("readonly") {
                    Some(toml::Value::Boolean(ro)) => Some(*ro),
                    Some(toml::Value::String(v)) => v.parse().ok(),
                    _ => None,
                }
            }()
            .unwrap_or(false),
        };
        if self
            .spec
            .storage_backends
            .insert(name.to_string(), backend_spec)
            .is_some()
        {
            return Err(SpecBuilderError::BackendNameInUse(name.to_string()));
        }
        Ok(())
    }

    fn add_storage_device_from_config(
        &mut self,
        name: &str,
        kind: StorageDeviceKind,
        device: &config::Device,
    ) -> Result<(), SpecBuilderError> {
        let backend_name = device
            .options
            .get("block_dev")
            .ok_or_else(|| {
                SpecBuilderError::ConfigTomlError(format!(
                    "No block_dev key for {}",
                    name
                ))
            })?
            .as_str()
            .ok_or_else(|| {
                SpecBuilderError::ConfigTomlError(format!(
                    "Couldn't parse block_dev for {}",
                    name
                ))
            })?;

        if !self.spec.storage_backends.contains_key(backend_name) {
            return Err(SpecBuilderError::ConfigTomlError(format!(
                "Couldn't find backend {} for storage device {}",
                backend_name, name
            )));
        }

        let pci_path: PciPath = device.get("pci-path").ok_or_else(|| {
            SpecBuilderError::ConfigTomlError(format!(
                "Failed to get PCI path for storage device {}",
                name
            ))
        })?;

        let device_spec = StorageDevice {
            kind,
            backend_name: backend_name.to_string(),
            pci_path,
        };

        if self
            .spec
            .storage_devices
            .insert(name.to_string(), device_spec)
            .is_some()
        {
            return Err(SpecBuilderError::DeviceNameInUse(name.to_string()));
        }
        Ok(())
    }

    fn add_network_device_from_config(
        &mut self,
        name: &str,
        device: &config::Device,
    ) -> Result<(), SpecBuilderError> {
        let vnic_name = device.get_string("vnic").ok_or_else(|| {
            SpecBuilderError::ConfigTomlError(format!(
                "Failed to parse vNIC name for device {}",
                name
            ))
        })?;
        let pci_path: PciPath = device.get("pci-path").ok_or_else(|| {
            SpecBuilderError::ConfigTomlError(format!(
                "Failed to get PCI path for network device {}",
                name
            ))
        })?;

        if self
            .spec
            .network_backends
            .insert(
                vnic_name.to_string(),
                NetworkBackend { vnic_name: vnic_name.to_string() },
            )
            .is_some()
        {
            return Err(SpecBuilderError::BackendNameInUse(
                vnic_name.to_string(),
            ));
        }

        if self
            .spec
            .network_devices
            .insert(
                name.to_string(),
                NetworkDevice { backend_name: vnic_name.to_string(), pci_path },
            )
            .is_some()
        {
            return Err(SpecBuilderError::DeviceNameInUse(name.to_string()));
        }

        Ok(())
    }

    fn add_pci_bridge_from_config(
        &mut self,
        bridge: &config::PciBridge,
    ) -> Result<(), SpecBuilderError> {
        let name = format!("pci-bridge-{}", bridge.downstream_bus);
        let pci_path = PciPath::from_str(&bridge.pci_path).map_err(|_| {
            SpecBuilderError::PciPathNotParseable(bridge.pci_path.clone())
        })?;

        if self
            .spec
            .pci_pci_bridges
            .insert(
                name,
                PciPciBridge {
                    downstream_bus: bridge.downstream_bus,
                    pci_path,
                },
            )
            .is_some()
        {
            return Err(SpecBuilderError::DeviceNameInUse(format!(
                "pci-bridge-{}",
                bridge.downstream_bus
            )));
        }

        Ok(())
    }

    /// Adds to the spec under construction all the devices and backends
    /// specified in the supplied configuration TOML.
    pub fn add_devices_from_config(
        &mut self,
        config: &config::Config,
    ) -> Result<(), SpecBuilderError> {
        // Initialize all the backends in the config file.
        for (name, backend) in config.block_devices() {
            self.add_storage_backend_from_config(name, backend)?;
        }
        for (name, device) in config.devs() {
            let driver = device.driver.as_str();
            match driver {
                "pci-virtio-block" => self.add_storage_device_from_config(
                    name,
                    StorageDeviceKind::Virtio,
                    device,
                )?,
                "pci-nvme" => self.add_storage_device_from_config(
                    name,
                    StorageDeviceKind::Nvme,
                    device,
                )?,
                "pci-virtio-viona" => {
                    self.add_network_device_from_config(name, device)?
                }
                _ => {
                    return Err(SpecBuilderError::ConfigTomlError(format!(
                        "Unrecognized device type {}",
                        driver
                    )))
                }
            }
        }
        for bridge in config.get_pci_bridges() {
            self.add_pci_bridge_from_config(bridge)?;
        }
        Ok(())
    }

    /// Adds to the spec under construction a serial port specification.
    pub fn add_serial_port(
        &mut self,
        port: SerialPortNumber,
        autodiscard: bool,
    ) -> Result<(), SpecBuilderError> {
        if self
            .spec
            .serial_ports
            .insert(
                match port {
                    SerialPortNumber::Com1 => "com1",
                    SerialPortNumber::Com2 => "com2",
                    SerialPortNumber::Com3 => "com3",
                    SerialPortNumber::Com4 => "com4",
                }
                .to_string(),
                SerialPort { num: port, autodiscard },
            )
            .is_some()
        {
            return Err(SpecBuilderError::SerialPortInUse(port));
        }
        Ok(())
    }

    pub fn finish(self) -> InstanceSpec {
        self.spec
    }
}

#[cfg(test)]
mod test {
    use std::{collections::BTreeMap, path::PathBuf};

    use propolis_client::api::Slot;

    use crate::config::{self, Config};

    use super::*;

    fn default_spec_builder() -> Result<SpecBuilder, SpecBuilderError> {
        SpecBuilder::new(
            &InstanceProperties {
                id: Default::default(),
                name: Default::default(),
                description: Default::default(),
                image_id: Default::default(),
                bootrom_id: Default::default(),
                memory: 512,
                vcpus: 4,
            },
            &Config::new(
                PathBuf::from_str("").unwrap(),
                config::Chipset::default(),
                BTreeMap::new(),
                BTreeMap::new(),
                Vec::new(),
            ),
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
                gen: 0,
                volume_construction_request: VolumeConstructionRequest::File {
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
                    gen: 0,
                    volume_construction_request:
                        VolumeConstructionRequest::File {
                            block_size: 512,
                            path: "disk2.img".to_string()
                        },
                })
                .err(),
            Some(SpecBuilderError::PciPathInUse(_))
        ));
    }

    #[test]
    fn duplicate_serial_port() {
        let mut builder = default_spec_builder().unwrap();
        assert!(builder.add_serial_port(SerialPortNumber::Com1, false).is_ok());
        assert!(builder.add_serial_port(SerialPortNumber::Com2, false).is_ok());
        assert!(builder.add_serial_port(SerialPortNumber::Com3, false).is_ok());
        assert!(builder.add_serial_port(SerialPortNumber::Com4, false).is_ok());
        assert!(matches!(
            builder.add_serial_port(SerialPortNumber::Com1, false).err(),
            Some(SpecBuilderError::SerialPortInUse(_))
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
                    gen: 0,
                    volume_construction_request:
                        VolumeConstructionRequest::File {
                            block_size: 512,
                            path: "disk3.img".to_string()
                        },
                })
                .err(),
            Some(SpecBuilderError::UnrecognizedStorageDevice(_))
        ));
    }
}
