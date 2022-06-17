//! Helper functions for building instance specs from server parameters.

use std::collections::BTreeSet;
use std::str::FromStr;

use propolis_client::api::{
    self, DiskRequest, InstanceProperties, NetworkInterfaceRequest,
};
use propolis_client::instance_spec::*;

use anyhow::Result;
use thiserror::Error;

use crate::config;

#[derive(Debug, Error)]
pub enum SpecBuilderError {
    #[error("A device with name {0} already exists")]
    DeviceNameInUse(String),

    #[error("A backend with name {0} already exists")]
    BackendNameInUse(String),

    #[error("A PCI device is already attached at {0:?}")]
    PciPathInUse(PciPath),

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

#[derive(Clone, Copy, Debug)]
pub enum SlotType {
    NIC,
    Disk,
    CloudInit,
}

/// Translates a device type and PCI slot (as presented in an instance creation
/// request) into a concrete PCI path.
fn slot_to_pci_path(slot: api::Slot, ty: SlotType) -> Result<PciPath> {
    match ty {
        // Slots for NICS: 0x08 -> 0x0F
        SlotType::NIC if slot.0 <= 7 => Ok(PciPath(0, slot.0 + 0x8, 0)),
        // Slots for Disks: 0x10 -> 0x17
        SlotType::Disk if slot.0 <= 7 => Ok(PciPath(0, slot.0 + 0x10, 0)),
        // Slot for CloudInit
        SlotType::CloudInit if slot.0 == 0 => Ok(PciPath(0, slot.0 + 0x18, 0)),
        _ => Err(SpecBuilderError::PciSlotInvalid(slot.0, ty).into()),
    }
}

pub struct SpecBuilder {
    spec: LatestInstanceSpec,
    pci_paths: BTreeSet<PciPath>,
}

impl SpecBuilder {
    pub fn new(
        properties: &InstanceProperties,
        config: &config::Config,
    ) -> Result<Self> {
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
            spec: LatestInstanceSpec {
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

    fn register_pci_device(&mut self, pci_path: PciPath) -> Result<()> {
        if self.pci_paths.contains(&pci_path) {
            Err(SpecBuilderError::PciPathInUse(pci_path).into())
        } else {
            self.pci_paths.insert(pci_path);
            Ok(())
        }
    }

    pub fn add_nic_from_request(
        &mut self,
        nic: &NetworkInterfaceRequest,
    ) -> Result<()> {
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
            )
            .into());
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
            )
            .into());
        }

        Ok(())
    }

    pub fn add_disk_from_request(&mut self, disk: &DiskRequest) -> Result<()> {
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
                        serialized_req: serde_json::to_string(
                            &disk.volume_construction_request,
                        )?,
                    },
                    readonly: disk.read_only,
                },
            )
            .is_some()
        {
            return Err(SpecBuilderError::BackendNameInUse(
                disk.name.to_string(),
            )
            .into());
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
                                )
                                .into(),
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
            )
            .into());
        }

        Ok(())
    }

    pub fn add_cloud_init_from_request(
        &mut self,
        cloud_init_bytes: &str,
    ) -> Result<()> {
        let name = "cloud-init";
        let pci_path = slot_to_pci_path(api::Slot(0), SlotType::CloudInit)?;
        self.register_pci_device(pci_path)?;
        let bytes = base64::decode(&cloud_init_bytes)?;

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
            return Err(
                SpecBuilderError::BackendNameInUse(name.to_string()).into()
            );
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
            return Err(
                SpecBuilderError::DeviceNameInUse(name.to_string()).into()
            );
        }

        Ok(())
    }

    fn add_storage_backend_from_config(
        &mut self,
        name: &str,
        backend: &config::BlockDevice,
    ) -> Result<()> {
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
                    )
                    .into())
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
            return Err(
                SpecBuilderError::BackendNameInUse(name.to_string()).into()
            );
        }
        Ok(())
    }

    fn add_storage_device_from_config(
        &mut self,
        name: &str,
        kind: StorageDeviceKind,
        device: &config::Device,
    ) -> Result<()> {
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
            ))
            .into());
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
            return Err(
                SpecBuilderError::DeviceNameInUse(name.to_string()).into()
            );
        }
        Ok(())
    }

    fn add_network_device_from_config(
        &mut self,
        name: &str,
        device: &config::Device,
    ) -> Result<()> {
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
            )
            .into());
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
            return Err(
                SpecBuilderError::DeviceNameInUse(name.to_string()).into()
            );
        }

        Ok(())
    }

    fn add_pci_bridge_from_config(
        &mut self,
        bridge: &config::PciBridge,
    ) -> Result<()> {
        let name = format!("pci-bridge-{}", bridge.downstream_bus);
        let pci_path = PciPath::from_str(&bridge.pci_path)?;
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
            ))
            .into());
        }

        Ok(())
    }

    pub fn add_devices_from_config(
        &mut self,
        config: &config::Config,
    ) -> Result<()> {
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
                    ))
                    .into())
                }
            }
        }
        for bridge in config.get_pci_bridges() {
            self.add_pci_bridge_from_config(bridge)?;
        }
        Ok(())
    }

    pub fn add_serial_port(
        &mut self,
        port: SerialPortNumber,
        autodiscard: bool,
    ) -> Result<()> {
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
            return Err(SpecBuilderError::SerialPortInUse(port).into());
        }
        Ok(())
    }

    pub fn finish(self) -> LatestInstanceSpec {
        self.spec
    }
}
