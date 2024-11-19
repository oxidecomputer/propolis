// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Functions for converting a [`super::Config`] into instance spec elements.

use std::{
    collections::BTreeMap,
    str::{FromStr, ParseBoolError},
};

use propolis_client::{
    types::{
        ComponentV0, DlpiNetworkBackend, FileStorageBackend,
        MigrationFailureInjector, NvmeDisk, P9fs, PciPciBridge, SoftNpuP9,
        SoftNpuPciPort, SoftNpuPort, VirtioDisk, VirtioNetworkBackend,
        VirtioNic,
    },
    PciPath, SpecKey,
};
use thiserror::Error;

pub const MIGRATION_FAILURE_DEVICE_NAME: &str = "test-migration-failure";

#[derive(Debug, Error)]
pub enum TomlToSpecError {
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

    #[error("failed to get source for p9 device {0:?}")]
    NoP9Source(String),

    #[error("failed to get source for p9 device {0:?}")]
    NoP9Target(String),
}

#[derive(Clone, Debug, Default)]
pub struct SpecConfig {
    pub enable_pcie: bool,
    pub components: BTreeMap<SpecKey, ComponentV0>,
}

impl TryFrom<&super::Config> for SpecConfig {
    type Error = TomlToSpecError;

    fn try_from(config: &super::Config) -> Result<Self, Self::Error> {
        let mut spec = SpecConfig {
            enable_pcie: config
                .chipset
                .options
                .get("enable-pcie")
                .map(|v| {
                    v.as_bool().ok_or_else(|| {
                        TomlToSpecError::EnablePcieParseFailed(v.to_string())
                    })
                })
                .transpose()?
                .unwrap_or(false),
            ..Default::default()
        };

        for (device_name, device) in config.devices.iter() {
            let device_id = SpecKey::from(device_name.clone());
            let driver = device.driver.as_str();
            if device_name == MIGRATION_FAILURE_DEVICE_NAME {
                const FAIL_EXPORTS: &str = "fail_exports";
                const FAIL_IMPORTS: &str = "fail_imports";
                let fail_exports = device
                    .options
                    .get(FAIL_EXPORTS)
                    .and_then(|val| val.as_integer())
                    .unwrap_or(0)
                    .max(0) as u32;
                let fail_imports = device
                    .options
                    .get(FAIL_IMPORTS)
                    .and_then(|val| val.as_integer())
                    .unwrap_or(0)
                    .max(0) as u32;

                spec.components.insert(
                    SpecKey::Name(MIGRATION_FAILURE_DEVICE_NAME.to_owned()),
                    ComponentV0::MigrationFailureInjector(
                        MigrationFailureInjector { fail_exports, fail_imports },
                    ),
                );

                continue;
            }

            match driver {
                // If this is a storage device, parse its "block_dev" property
                // to get the name of its corresponding backend.
                "pci-virtio-block" | "pci-nvme" => {
                    let (device_spec, backend_id) =
                        parse_storage_device_from_config(device_name, device)?;

                    let backend_name = backend_id.to_string();
                    let backend_config =
                        config.block_devs.get(&backend_name).ok_or_else(
                            || TomlToSpecError::StorageDeviceBackendNotFound {
                                device: device_name.to_owned(),
                                backend: backend_name.to_string(),
                            },
                        )?;

                    let backend_spec = parse_storage_backend_from_config(
                        &backend_name,
                        backend_config,
                    )?;

                    spec.components.insert(device_id, device_spec);
                    spec.components.insert(backend_id, backend_spec);
                }
                "pci-virtio-viona" => {
                    let ParsedNic { device_spec, backend_spec, backend_id } =
                        parse_network_device_from_config(device_name, device)?;

                    spec.components
                        .insert(device_id, ComponentV0::VirtioNic(device_spec));

                    spec.components.insert(
                        backend_id,
                        ComponentV0::VirtioNetworkBackend(backend_spec),
                    );
                }
                "softnpu-pci-port" => {
                    let pci_path: PciPath =
                        device.get("pci-path").ok_or_else(|| {
                            TomlToSpecError::InvalidPciPath(
                                device_name.to_owned(),
                            )
                        })?;

                    spec.components.insert(
                        device_id,
                        ComponentV0::SoftNpuPciPort(SoftNpuPciPort {
                            pci_path,
                        }),
                    );
                }
                "softnpu-port" => {
                    let vnic_name =
                        device.get_string("vnic").ok_or_else(|| {
                            TomlToSpecError::NoVnicName(device_name.to_owned())
                        })?;

                    let backend_name =
                        SpecKey::Name(format!("{device_id}:backend"));

                    spec.components.insert(
                        device_id,
                        ComponentV0::SoftNpuPort(SoftNpuPort {
                            link_name: device_name.to_string(),
                            backend_id: backend_name.clone(),
                        }),
                    );

                    spec.components.insert(
                        backend_name,
                        ComponentV0::DlpiNetworkBackend(DlpiNetworkBackend {
                            vnic_name: vnic_name.to_owned(),
                        }),
                    );
                }
                "softnpu-p9" => {
                    let pci_path: PciPath =
                        device.get("pci-path").ok_or_else(|| {
                            TomlToSpecError::InvalidPciPath(
                                device_name.to_owned(),
                            )
                        })?;

                    spec.components.insert(
                        device_id,
                        ComponentV0::SoftNpuP9(SoftNpuP9 { pci_path }),
                    );
                }
                "pci-virtio-9p" => {
                    spec.components.insert(
                        device_id,
                        ComponentV0::P9fs(parse_p9fs_from_config(
                            device_name,
                            device,
                        )?),
                    );
                }
                _ => {
                    return Err(TomlToSpecError::UnrecognizedDeviceType(
                        driver.to_owned(),
                    ))
                }
            }
        }

        for bridge in config.pci_bridges.iter() {
            let pci_path =
                PciPath::from_str(&bridge.pci_path).map_err(|e| {
                    TomlToSpecError::PciPathParseFailed(
                        bridge.pci_path.to_string(),
                        e,
                    )
                })?;

            spec.components.insert(
                SpecKey::Name(format!("pci-bridge-{}", bridge.pci_path)),
                ComponentV0::PciPciBridge(PciPciBridge {
                    downstream_bus: bridge.downstream_bus,
                    pci_path,
                }),
            );
        }

        Ok(spec)
    }
}

fn parse_storage_device_from_config(
    name: &str,
    device: &super::Device,
) -> Result<(ComponentV0, SpecKey), TomlToSpecError> {
    enum Interface {
        Virtio,
        Nvme,
    }

    let interface = match device.driver.as_str() {
        "pci-virtio-block" => Interface::Virtio,
        "pci-nvme" => Interface::Nvme,
        _ => {
            return Err(TomlToSpecError::InvalidStorageDeviceType {
                kind: device.driver.clone(),
                name: name.to_owned(),
            });
        }
    };

    let backend_id = SpecKey::from_str(
        device
            .options
            .get("block_dev")
            .ok_or_else(|| {
                TomlToSpecError::NoBackendNameForStorageDevice(name.to_owned())
            })?
            .as_str()
            .ok_or_else(|| {
                TomlToSpecError::NoBackendNameForStorageDevice(name.to_owned())
            })?,
    )
    .expect("SpecKey::from_str is infallible");

    let pci_path: PciPath = device
        .get("pci-path")
        .ok_or_else(|| TomlToSpecError::InvalidPciPath(name.to_owned()))?;

    let id_to_return = backend_id.clone();
    Ok((
        match interface {
            Interface::Virtio => {
                ComponentV0::VirtioDisk(VirtioDisk { backend_id, pci_path })
            }
            Interface::Nvme => {
                ComponentV0::NvmeDisk(NvmeDisk { backend_id, pci_path })
            }
        },
        id_to_return,
    ))
}

fn parse_storage_backend_from_config(
    name: &str,
    backend: &super::BlockDevice,
) -> Result<ComponentV0, TomlToSpecError> {
    let backend_spec = match backend.bdtype.as_str() {
        "file" => ComponentV0::FileStorageBackend(FileStorageBackend {
            path: backend
                .options
                .get("path")
                .ok_or_else(|| {
                    TomlToSpecError::InvalidFileBackendPath(name.to_owned())
                })?
                .as_str()
                .ok_or_else(|| {
                    TomlToSpecError::InvalidFileBackendPath(name.to_owned())
                })?
                .to_string(),
            readonly: match backend.options.get("readonly") {
                Some(toml::Value::Boolean(ro)) => Some(*ro),
                Some(toml::Value::String(v)) => {
                    Some(v.parse::<bool>().map_err(|e| {
                        TomlToSpecError::FileBackendReadonlyParseFailed(
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
            return Err(TomlToSpecError::InvalidStorageBackendType {
                kind: backend.bdtype.clone(),
                name: name.to_owned(),
            });
        }
    };

    Ok(backend_spec)
}

struct ParsedNic {
    device_spec: VirtioNic,
    backend_spec: VirtioNetworkBackend,
    backend_id: SpecKey,
}

fn parse_network_device_from_config(
    name: &str,
    device: &super::Device,
) -> Result<ParsedNic, TomlToSpecError> {
    let vnic_name = device
        .get_string("vnic")
        .ok_or_else(|| TomlToSpecError::NoVnicName(name.to_owned()))?;

    let pci_path: PciPath = device
        .get("pci-path")
        .ok_or_else(|| TomlToSpecError::InvalidPciPath(name.to_owned()))?;

    let backend_id = SpecKey::Name(format!("{name}-backend"));
    Ok(ParsedNic {
        device_spec: VirtioNic {
            backend_id: backend_id.clone(),
            interface_id: uuid::Uuid::nil(),
            pci_path,
        },
        backend_spec: VirtioNetworkBackend { vnic_name: vnic_name.to_owned() },
        backend_id,
    })
}

fn parse_p9fs_from_config(
    name: &str,
    device: &super::Device,
) -> Result<P9fs, TomlToSpecError> {
    let source = device
        .get_string("source")
        .ok_or_else(|| TomlToSpecError::NoP9Source(name.to_owned()))?;
    let target = device
        .get_string("target")
        .ok_or_else(|| TomlToSpecError::NoP9Target(name.to_owned()))?;
    let pci_path: PciPath = device
        .get("pci-path")
        .ok_or_else(|| TomlToSpecError::InvalidPciPath(name.to_owned()))?;

    let chunk_size = device.get("chunk_size").unwrap_or(65536);
    Ok(P9fs {
        source: source.to_owned(),
        target: target.to_owned(),
        chunk_size,
        pci_path,
    })
}
