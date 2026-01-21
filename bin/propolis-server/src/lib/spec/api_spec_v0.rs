// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Conversions from version-0 instance specs in the [`propolis_api_types`]
//! crate to the internal [`super::Spec`] representation.

use std::collections::BTreeMap;

use propolis_api_types::instance_spec::{
    components::{
        backends::{DlpiNetworkBackend, VirtioNetworkBackend},
        board::Board as InstanceSpecBoard,
        devices::{BootSettings, SerialPort as SerialPortDesc},
    },
    SpecKey,
};
use propolis_api_types_versions::v1;
use thiserror::Error;

#[cfg(feature = "falcon")]
use propolis_api_types::instance_spec::components::devices::SoftNpuPort as SoftNpuPortSpec;

use super::{
    builder::{SpecBuilder, SpecBuilderError},
    Disk, Nic, QemuPvpanic, SerialPortDevice, Spec, StorageBackend,
    StorageDevice,
};

#[cfg(feature = "failure-injection")]
use super::MigrationFailure;

#[cfg(feature = "falcon")]
use super::SoftNpuPort;

#[derive(Debug, Error)]
pub(crate) enum ApiSpecError {
    #[error(transparent)]
    Builder(#[from] SpecBuilderError),

    #[error("storage backend {backend} not found for device {device}")]
    StorageBackendNotFound { backend: SpecKey, device: SpecKey },

    #[error("network backend {backend} not found for device {device}")]
    NetworkBackendNotFound { backend: SpecKey, device: SpecKey },

    #[allow(dead_code)]
    #[error("support for component {component} compiled out via {feature}")]
    FeatureCompiledOut { component: SpecKey, feature: &'static str },

    #[error("backend {0} not used by any device")]
    BackendNotUsed(SpecKey),
}

impl From<Spec> for v1::instance_spec::InstanceSpec {
    fn from(val: Spec) -> Self {
        // Exhaustively destructure the input spec so that adding a new field
        // without considering it here will break the build.
        let Spec {
            board,
            cpuid,
            disks,
            nics,
            boot_settings,
            serial,
            pci_pci_bridges,
            pvpanic,
            #[cfg(feature = "failure-injection")]
            migration_failure,
            #[cfg(feature = "falcon")]
            softnpu,

            // Not part of `v1::instance_spec::InstanceSpec`. Added in `InstanceSpec` in API
            // Version 2.0.0.
            smbios_type1_input: _,
        } = val;

        // Inserts a component entry into the supplied map, asserting first that
        // the supplied key is not present in that map.
        //
        // This assertion is valid because internal instance specs should assign
        // a unique name to each component they describe. The spec builder
        // upholds this invariant at spec creation time.
        #[track_caller]
        fn insert_component(
            spec: &mut v1::instance_spec::InstanceSpec,
            key: SpecKey,
            val: v1::instance_spec::Component,
        ) {
            assert!(
                !spec.components.contains_key(&key),
                "component name {} already exists in output spec",
                &key
            );
            spec.components.insert(key, val);
        }

        let board = InstanceSpecBoard {
            cpus: board.cpus,
            memory_mb: board.memory_mb,
            chipset: board.chipset,
            guest_hv_interface: board.guest_hv_interface,
            cpuid: Some(cpuid.into_instance_spec_cpuid()),
        };
        let mut spec = v1::instance_spec::InstanceSpec {
            board,
            components: Default::default(),
        };

        for (disk_id, disk) in disks {
            let backend_id = disk.device_spec.backend_id().to_owned();
            insert_component(&mut spec, disk_id, disk.device_spec.into());
            insert_component(&mut spec, backend_id, disk.backend_spec.into());
        }

        for (nic_id, nic) in nics {
            let backend_id = nic.device_spec.backend_id.clone();
            insert_component(
                &mut spec,
                nic_id,
                v1::instance_spec::Component::VirtioNic(nic.device_spec),
            );

            insert_component(
                &mut spec,
                backend_id,
                v1::instance_spec::Component::VirtioNetworkBackend(
                    nic.backend_spec,
                ),
            );
        }

        for (name, desc) in serial {
            if desc.device == SerialPortDevice::Uart {
                insert_component(
                    &mut spec,
                    name,
                    v1::instance_spec::Component::SerialPort(SerialPortDesc {
                        num: desc.num,
                    }),
                );
            }
        }

        for (bridge_name, bridge) in pci_pci_bridges {
            insert_component(
                &mut spec,
                bridge_name,
                v1::instance_spec::Component::PciPciBridge(bridge),
            );
        }

        if let Some(pvpanic) = pvpanic {
            insert_component(
                &mut spec,
                pvpanic.id,
                v1::instance_spec::Component::QemuPvpanic(pvpanic.spec),
            );
        }

        if let Some(settings) = boot_settings {
            insert_component(
                &mut spec,
                settings.name,
                v1::instance_spec::Component::BootSettings(BootSettings {
                    order: settings.order.into_iter().map(Into::into).collect(),
                }),
            );
        }

        #[cfg(feature = "failure-injection")]
        if let Some(mig) = migration_failure {
            insert_component(
                &mut spec,
                mig.id,
                v1::instance_spec::Component::MigrationFailureInjector(
                    mig.spec,
                ),
            );
        }

        #[cfg(feature = "falcon")]
        {
            if let Some(softnpu_pci) = softnpu.pci_port {
                insert_component(
                    &mut spec,
                    SpecKey::Name(format!(
                        "softnpu-pci-{}",
                        softnpu_pci.pci_path
                    )),
                    v1::instance_spec::Component::SoftNpuPciPort(softnpu_pci),
                );
            }

            if let Some(p9) = softnpu.p9_device {
                insert_component(
                    &mut spec,
                    SpecKey::Name(format!("softnpu-p9-{}", p9.pci_path)),
                    v1::instance_spec::Component::SoftNpuP9(p9),
                );
            }

            if let Some(p9fs) = softnpu.p9fs {
                insert_component(
                    &mut spec,
                    SpecKey::Name(format!("p9fs-{}", p9fs.pci_path)),
                    v1::instance_spec::Component::P9fs(p9fs),
                );
            }

            for (port_name, port) in softnpu.ports {
                insert_component(
                    &mut spec,
                    port_name.clone(),
                    v1::instance_spec::Component::SoftNpuPort(
                        SoftNpuPortSpec {
                            link_name: port.link_name,
                            backend_id: port.backend_name.clone(),
                        },
                    ),
                );

                insert_component(
                    &mut spec,
                    port.backend_name,
                    v1::instance_spec::Component::DlpiNetworkBackend(
                        port.backend_spec,
                    ),
                );
            }
        }

        spec
    }
}

impl TryFrom<v1::instance_spec::InstanceSpec> for Spec {
    type Error = ApiSpecError;

    fn try_from(
        value: v1::instance_spec::InstanceSpec,
    ) -> Result<Self, Self::Error> {
        let mut builder = SpecBuilder::with_instance_spec_board(value.board)?;
        let mut devices: Vec<(SpecKey, v1::instance_spec::Component)> = vec![];
        let mut boot_settings = None;
        let mut storage_backends: BTreeMap<SpecKey, StorageBackend> =
            BTreeMap::new();
        let mut viona_backends: BTreeMap<SpecKey, VirtioNetworkBackend> =
            BTreeMap::new();
        let mut dlpi_backends: BTreeMap<SpecKey, DlpiNetworkBackend> =
            BTreeMap::new();

        for (id, component) in value.components.into_iter() {
            match component {
                v1::instance_spec::Component::CrucibleStorageBackend(_)
                | v1::instance_spec::Component::FileStorageBackend(_)
                | v1::instance_spec::Component::BlobStorageBackend(_) => {
                    storage_backends.insert(
                        id,
                        component.try_into().expect(
                            "component is known to be a storage backend",
                        ),
                    );
                }
                v1::instance_spec::Component::VirtioNetworkBackend(viona) => {
                    viona_backends.insert(id, viona);
                }
                v1::instance_spec::Component::DlpiNetworkBackend(dlpi) => {
                    dlpi_backends.insert(id, dlpi);
                }
                device => {
                    devices.push((id, device));
                }
            }
        }

        for (device_id, device_spec) in devices {
            match device_spec {
                v1::instance_spec::Component::VirtioDisk(_)
                | v1::instance_spec::Component::NvmeDisk(_) => {
                    let device_spec = StorageDevice::try_from(device_spec)
                        .expect("component is known to be a disk");

                    let (_, backend_spec) = storage_backends
                        .remove_entry(device_spec.backend_id())
                        .ok_or_else(|| {
                            ApiSpecError::StorageBackendNotFound {
                                backend: device_spec.backend_id().to_owned(),
                                device: device_id.clone(),
                            }
                        })?;

                    builder.add_storage_device(
                        device_id,
                        Disk { device_spec, backend_spec },
                    )?;
                }
                v1::instance_spec::Component::VirtioNic(nic) => {
                    let (_, backend_spec) = viona_backends
                        .remove_entry(&nic.backend_id)
                        .ok_or_else(|| {
                            ApiSpecError::NetworkBackendNotFound {
                                backend: nic.backend_id.clone(),
                                device: device_id.clone(),
                            }
                        })?;

                    builder.add_network_device(
                        device_id,
                        Nic { device_spec: nic, backend_spec },
                    )?;
                }
                v1::instance_spec::Component::SerialPort(port) => {
                    builder.add_serial_port(device_id, port.num)?;
                }
                v1::instance_spec::Component::PciPciBridge(bridge) => {
                    builder.add_pci_bridge(device_id, bridge)?;
                }
                v1::instance_spec::Component::QemuPvpanic(pvpanic) => {
                    builder.add_pvpanic_device(QemuPvpanic {
                        id: device_id,
                        spec: pvpanic,
                    })?;
                }
                v1::instance_spec::Component::BootSettings(settings) => {
                    // The builder returns an error if its caller tries to add
                    // a boot option that isn't in the set of attached disks.
                    // Since there may be more disk devices left in the
                    // component map, just capture the boot order for now and
                    // apply it to the builder later.
                    boot_settings = Some((device_id, settings));
                }
                #[cfg(not(feature = "failure-injection"))]
                v1::instance_spec::Component::MigrationFailureInjector(_) => {
                    return Err(ApiSpecError::FeatureCompiledOut {
                        component: device_id,
                        feature: "failure-injection",
                    });
                }
                #[cfg(feature = "failure-injection")]
                v1::instance_spec::Component::MigrationFailureInjector(mig) => {
                    builder.add_migration_failure_device(MigrationFailure {
                        id: device_id,
                        spec: mig,
                    })?;
                }
                #[cfg(not(feature = "falcon"))]
                v1::instance_spec::Component::SoftNpuPciPort(_)
                | v1::instance_spec::Component::SoftNpuPort(_)
                | v1::instance_spec::Component::SoftNpuP9(_)
                | v1::instance_spec::Component::P9fs(_) => {
                    return Err(ApiSpecError::FeatureCompiledOut {
                        component: device_id,
                        feature: "falcon",
                    });
                }
                #[cfg(feature = "falcon")]
                v1::instance_spec::Component::SoftNpuPciPort(port) => {
                    builder.set_softnpu_pci_port(port)?;
                }
                #[cfg(feature = "falcon")]
                v1::instance_spec::Component::SoftNpuPort(port) => {
                    let (_, backend_spec) = dlpi_backends
                        .remove_entry(&port.backend_id)
                        .ok_or_else(|| {
                            ApiSpecError::NetworkBackendNotFound {
                                backend: port.backend_id.clone(),
                                device: device_id.clone(),
                            }
                        })?;

                    let port = SoftNpuPort {
                        link_name: port.link_name,
                        backend_name: port.backend_id,
                        backend_spec,
                    };

                    builder.add_softnpu_port(device_id, port)?;
                }
                #[cfg(feature = "falcon")]
                v1::instance_spec::Component::SoftNpuP9(p9) => {
                    builder.set_softnpu_p9(p9)?;
                }
                #[cfg(feature = "falcon")]
                v1::instance_spec::Component::P9fs(p9fs) => {
                    builder.set_p9fs(p9fs)?;
                }
                v1::instance_spec::Component::CrucibleStorageBackend(_)
                | v1::instance_spec::Component::FileStorageBackend(_)
                | v1::instance_spec::Component::BlobStorageBackend(_)
                | v1::instance_spec::Component::VirtioNetworkBackend(_)
                | v1::instance_spec::Component::DlpiNetworkBackend(_) => {
                    unreachable!("already filtered out backends")
                }
            }
        }

        // Now that all disks have been attached, try to establish the boot
        // order if one was supplied.
        if let Some(settings) = boot_settings {
            builder.add_boot_order(
                settings.0,
                settings.1.order.into_iter().map(Into::into),
            )?;
        }

        if let Some(backend) = storage_backends.into_keys().next() {
            return Err(ApiSpecError::BackendNotUsed(backend));
        }

        if let Some(backend) = viona_backends.into_keys().next() {
            return Err(ApiSpecError::BackendNotUsed(backend));
        }

        if let Some(backend) = dlpi_backends.into_keys().next() {
            return Err(ApiSpecError::BackendNotUsed(backend));
        }

        Ok(builder.finish())
    }
}
