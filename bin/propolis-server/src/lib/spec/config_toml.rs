// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Functions for converting a config TOML into instance spec elements.

use std::str::{FromStr, ParseBoolError};

use propolis_api_types::instance_spec::{
    components::{
        backends::{FileStorageBackend, VirtioNetworkBackend},
        devices::{NvmeDisk, PciPciBridge, VirtioDisk, VirtioNic},
    },
    PciPath,
};
use thiserror::Error;

#[cfg(feature = "falcon")]
use propolis_api_types::instance_spec::components::devices::{
    P9fs, SoftNpuP9, SoftNpuPciPort,
};

use crate::config;

use super::{
    Disk, Nic, ParsedDiskRequest, ParsedNicRequest, ParsedPciBridgeRequest,
    StorageBackend, StorageDevice,
};

#[cfg(feature = "falcon")]
use super::{ParsedSoftNpu, ParsedSoftNpuPort, SoftNpuPort};

#[derive(Debug, Error)]
pub(crate) enum ConfigTomlError {
    #[error("unrecognized device type {0:?}")]
    UnrecognizedDeviceType(String),

    #[error("invalid value {0:?} for enable-pcie flag in chipset")]
    EnablePcieParseFailed(String),

    #[error("failed to get PCI path for device {0:?}")]
    InvalidPciPath(String),

    #[error("failed to parse PCI path string {0:?}")]
    PciPathParseFailed(String, #[source] std::io::Error),

    #[error("invalid storage device kind {kind:?} for device {name:?}")]
    InvalidStorageDeviceType { kind: String, name: String },

    #[error("no backend name for storage device {0:?}")]
    NoBackendNameForStorageDevice(String),

    #[error("invalid storage backend kind {kind:?} for backend {name:?}")]
    InvalidStorageBackendType { kind: String, name: String },

    #[error("couldn't find storage device {device:?}'s backend {backend:?}")]
    StorageDeviceBackendNotFound { device: String, backend: String },

    #[error("couldn't get path for file backend {0:?}")]
    InvalidFileBackendPath(String),

    #[error("failed to parse read-only option for file backend {0:?}")]
    FileBackendReadonlyParseFailed(String, #[source] ParseBoolError),

    #[error("failed to get VNIC name for device {0:?}")]
    NoVnicName(String),

    #[cfg(feature = "falcon")]
    #[error("failed to get source for p9 device {0:?}")]
    NoP9Source(String),

    #[cfg(feature = "falcon")]
    #[error("failed to get source for p9 device {0:?}")]
    NoP9Target(String),
}

#[derive(Default)]
pub(super) struct ParsedConfig {
    pub(super) enable_pcie: bool,
    pub(super) disks: Vec<ParsedDiskRequest>,
    pub(super) nics: Vec<ParsedNicRequest>,
    pub(super) pci_bridges: Vec<ParsedPciBridgeRequest>,

    #[cfg(feature = "falcon")]
    pub(super) softnpu: ParsedSoftNpu,
}

impl TryFrom<&config::Config> for ParsedConfig {
    type Error = ConfigTomlError;

    fn try_from(config: &config::Config) -> Result<Self, Self::Error> {
        let mut parsed = ParsedConfig {
            enable_pcie: config
                .chipset
                .options
                .get("enable-pcie")
                .map(|v| {
                    v.as_bool().ok_or_else(|| {
                        ConfigTomlError::EnablePcieParseFailed(v.to_string())
                    })
                })
                .transpose()?
                .unwrap_or(false),
            ..Default::default()
        };

        for (device_name, device) in config.devices.iter() {
            let driver = device.driver.as_str();
            match driver {
                // If this is a storage device, parse its "block_dev" property
                // to get the name of its corresponding backend.
                "pci-virtio-block" | "pci-nvme" => {
                    let device_spec =
                        parse_storage_device_from_config(device_name, device)?;

                    let backend_name = match &device_spec {
                        StorageDevice::Virtio(disk) => {
                            disk.backend_name.clone()
                        }
                        StorageDevice::Nvme(disk) => disk.backend_name.clone(),
                    };

                    let backend_config =
                        config.block_devs.get(&backend_name).ok_or_else(
                            || ConfigTomlError::StorageDeviceBackendNotFound {
                                device: device_name.to_owned(),
                                backend: backend_name.to_owned(),
                            },
                        )?;

                    let backend_spec = parse_storage_backend_from_config(
                        &backend_name,
                        backend_config,
                    )?;

                    parsed.disks.push(ParsedDiskRequest {
                        name: device_name.to_owned(),
                        disk: Disk { device_spec, backend_name, backend_spec },
                    });
                }
                "pci-virtio-viona" => {
                    parsed.nics.push(parse_network_device_from_config(
                        device_name,
                        device,
                    )?);
                }
                #[cfg(feature = "falcon")]
                "softnpu-pci-port" => {
                    parsed.softnpu.pci_port =
                        Some(parse_softnpu_pci_port_from_config(
                            device_name,
                            device,
                        )?);
                }
                #[cfg(feature = "falcon")]
                "softnpu-port" => {
                    parsed.softnpu.ports.push(parse_softnpu_port_from_config(
                        device_name,
                        device,
                    )?);
                }
                #[cfg(feature = "falcon")]
                "softnpu-p9" => {
                    parsed.softnpu.p9_device = Some(
                        parse_softnpu_p9_from_config(device_name, device)?,
                    );
                }
                #[cfg(feature = "falcon")]
                "pci-virtio-9p" => {
                    parsed.softnpu.p9fs =
                        Some(parse_p9fs_from_config(device_name, device)?);
                }
                _ => {
                    return Err(ConfigTomlError::UnrecognizedDeviceType(
                        driver.to_owned(),
                    ))
                }
            }
        }

        for bridge in config.pci_bridges.iter() {
            parsed.pci_bridges.push(parse_pci_bridge_from_config(bridge)?);
        }

        Ok(parsed)
    }
}

pub(super) fn parse_storage_backend_from_config(
    name: &str,
    backend: &config::BlockDevice,
) -> Result<StorageBackend, ConfigTomlError> {
    let backend_spec = match backend.bdtype.as_str() {
        "file" => StorageBackend::File(FileStorageBackend {
            path: backend
                .options
                .get("path")
                .ok_or_else(|| {
                    ConfigTomlError::InvalidFileBackendPath(name.to_owned())
                })?
                .as_str()
                .ok_or_else(|| {
                    ConfigTomlError::InvalidFileBackendPath(name.to_owned())
                })?
                .to_string(),
            readonly: match backend.options.get("readonly") {
                Some(toml::Value::Boolean(ro)) => Some(*ro),
                Some(toml::Value::String(v)) => {
                    Some(v.parse::<bool>().map_err(|e| {
                        ConfigTomlError::FileBackendReadonlyParseFailed(
                            name.to_owned(),
                            e,
                        )
                    })?)
                }
                _ => None,
            }
            .unwrap_or(false),
        }),
        _ => {
            return Err(ConfigTomlError::InvalidStorageBackendType {
                kind: backend.bdtype.clone(),
                name: name.to_owned(),
            });
        }
    };

    Ok(backend_spec)
}

pub(super) fn parse_storage_device_from_config(
    name: &str,
    device: &config::Device,
) -> Result<StorageDevice, ConfigTomlError> {
    enum Interface {
        Virtio,
        Nvme,
    }

    let interface = match device.driver.as_str() {
        "pci-virtio-block" => Interface::Virtio,
        "pci-nvme" => Interface::Nvme,
        _ => {
            return Err(ConfigTomlError::InvalidStorageDeviceType {
                kind: device.driver.clone(),
                name: name.to_owned(),
            });
        }
    };

    let backend_name = device
        .options
        .get("block_dev")
        .ok_or_else(|| {
            ConfigTomlError::NoBackendNameForStorageDevice(name.to_owned())
        })?
        .as_str()
        .ok_or_else(|| {
            ConfigTomlError::NoBackendNameForStorageDevice(name.to_owned())
        })?
        .to_owned();

    let pci_path: PciPath = device
        .get("pci-path")
        .ok_or_else(|| ConfigTomlError::InvalidPciPath(name.to_owned()))?;

    Ok(match interface {
        Interface::Virtio => {
            StorageDevice::Virtio(VirtioDisk { backend_name, pci_path })
        }
        Interface::Nvme => {
            StorageDevice::Nvme(NvmeDisk { backend_name, pci_path })
        }
    })
}

pub(super) fn parse_network_device_from_config(
    name: &str,
    device: &config::Device,
) -> Result<ParsedNicRequest, ConfigTomlError> {
    let vnic_name = device
        .get_string("vnic")
        .ok_or_else(|| ConfigTomlError::NoVnicName(name.to_owned()))?;

    let pci_path: PciPath = device
        .get("pci-path")
        .ok_or_else(|| ConfigTomlError::InvalidPciPath(name.to_owned()))?;

    let (device_name, backend_name) = super::pci_path_to_nic_names(pci_path);
    let backend_spec = VirtioNetworkBackend { vnic_name: vnic_name.to_owned() };
    let device_spec = VirtioNic {
        backend_name: backend_name.clone(),
        // NICs added by the configuration TOML have no control plane-
        // supplied correlation IDs.
        interface_id: uuid::Uuid::nil(),
        pci_path,
    };

    Ok(ParsedNicRequest {
        name: device_name,
        nic: Nic { device_spec, backend_name, backend_spec },
    })
}

pub(super) fn parse_pci_bridge_from_config(
    bridge: &config::PciBridge,
) -> Result<ParsedPciBridgeRequest, ConfigTomlError> {
    let pci_path = PciPath::from_str(&bridge.pci_path).map_err(|e| {
        ConfigTomlError::PciPathParseFailed(bridge.pci_path.to_string(), e)
    })?;

    let name = format!("pci-bridge-{}", bridge.pci_path);
    Ok(ParsedPciBridgeRequest {
        name,
        bridge: PciPciBridge {
            downstream_bus: bridge.downstream_bus,
            pci_path,
        },
    })
}

#[cfg(feature = "falcon")]
pub(super) fn parse_softnpu_p9_from_config(
    name: &str,
    device: &config::Device,
) -> Result<SoftNpuP9, ConfigTomlError> {
    let pci_path: PciPath = device
        .get("pci-path")
        .ok_or_else(|| ConfigTomlError::InvalidPciPath(name.to_owned()))?;

    Ok(SoftNpuP9 { pci_path })
}

#[cfg(feature = "falcon")]
pub(super) fn parse_softnpu_pci_port_from_config(
    name: &str,
    device: &config::Device,
) -> Result<SoftNpuPciPort, ConfigTomlError> {
    let pci_path: PciPath = device
        .get("pci-path")
        .ok_or_else(|| ConfigTomlError::InvalidPciPath(name.to_owned()))?;

    Ok(SoftNpuPciPort { pci_path })
}

#[cfg(feature = "falcon")]
pub(super) fn parse_softnpu_port_from_config(
    name: &str,
    device: &config::Device,
) -> Result<ParsedSoftNpuPort, ConfigTomlError> {
    use propolis_api_types::instance_spec::components::backends::DlpiNetworkBackend;

    let vnic_name = device
        .get_string("vnic")
        .ok_or_else(|| ConfigTomlError::NoVnicName(name.to_owned()))?;

    Ok(ParsedSoftNpuPort {
        name: name.to_owned(),
        port: SoftNpuPort {
            backend_name: vnic_name.to_owned(),
            backend_spec: DlpiNetworkBackend {
                vnic_name: vnic_name.to_owned(),
            },
        },
    })
}

#[cfg(feature = "falcon")]
pub(super) fn parse_p9fs_from_config(
    name: &str,
    device: &config::Device,
) -> Result<P9fs, ConfigTomlError> {
    let source = device
        .get_string("source")
        .ok_or_else(|| ConfigTomlError::NoP9Source(name.to_owned()))?;
    let target = device
        .get_string("target")
        .ok_or_else(|| ConfigTomlError::NoP9Target(name.to_owned()))?;
    let pci_path: PciPath = device
        .get("pci-path")
        .ok_or_else(|| ConfigTomlError::InvalidPciPath(name.to_owned()))?;

    let chunk_size = device.get("chunk_size").unwrap_or(65536);
    Ok(P9fs {
        source: source.to_owned(),
        target: target.to_owned(),
        chunk_size,
        pci_path,
    })
}
