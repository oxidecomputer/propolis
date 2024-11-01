// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Conversions from version-0 instance specs in the [`propolis_api_types`]
//! crate to the internal [`super::Spec`] representation.

use std::collections::HashMap;

use propolis_api_types::instance_spec::{
    components::{
        backends::{DlpiNetworkBackend, VirtioNetworkBackend},
        board::Board as InstanceSpecBoard,
        devices::{BootSettings, SerialPort as SerialPortDesc},
    },
    v0::{ComponentV0, InstanceSpecV0},
    SpecKey,
};
use thiserror::Error;

#[cfg(feature = "falcon")]
use propolis_api_types::instance_spec::components::devices::SoftNpuPort as SoftNpuPortSpec;

use super::{
    builder::{SpecBuilder, SpecBuilderError},
    Disk, Nic, QemuPvpanic, SerialPortDevice, Spec, StorageBackend,
    StorageDevice,
};

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
    #[error("component {0} disabled by compile-time options")]
    FeatureCompiledOut(SpecKey),

    #[error("backend {0} not used by any device")]
    BackendNotUsed(SpecKey),
}

impl From<Spec> for InstanceSpecV0 {
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

            #[cfg(not(feature = "omicron-build"))]
            migration_failure,

            #[cfg(feature = "falcon")]
            softnpu,
        } = val;

        // Inserts a component entry into the supplied map, asserting first that
        // the supplied key is not present in that map.
        //
        // This assertion is valid because internal instance specs should assign
        // a unique name to each component they describe. The spec builder
        // upholds this invariant at spec creation time.
        #[track_caller]
        fn insert_component(
            spec: &mut InstanceSpecV0,
            key: SpecKey,
            val: ComponentV0,
        ) {
            assert!(
                !spec.components.contains_key(&key),
                "component {} already exists in output spec",
                &key
            );
            spec.components.insert(key, val);
        }

        let board = InstanceSpecBoard {
            cpus: board.cpus,
            memory_mb: board.memory_mb,
            chipset: board.chipset,
            cpuid: cpuid.map(|set| set.into_instance_spec_cpuid()),
        };
        let mut spec = InstanceSpecV0 { board, components: Default::default() };

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
                ComponentV0::VirtioNic(nic.device_spec),
            );

            insert_component(
                &mut spec,
                backend_id,
                ComponentV0::VirtioNetworkBackend(nic.backend_spec),
            );
        }

        for (name, desc) in serial {
            if desc.device == SerialPortDevice::Uart {
                insert_component(
                    &mut spec,
                    name,
                    ComponentV0::SerialPort(SerialPortDesc { num: desc.num }),
                );
            }
        }

        for (bridge_name, bridge) in pci_pci_bridges {
            insert_component(
                &mut spec,
                bridge_name,
                ComponentV0::PciPciBridge(bridge),
            );
        }

        if let Some(pvpanic) = pvpanic {
            insert_component(
                &mut spec,
                pvpanic.id,
                ComponentV0::QemuPvpanic(pvpanic.spec),
            );
        }

        if let Some(settings) = boot_settings {
            insert_component(
                &mut spec,
                settings.component_id,
                ComponentV0::BootSettings(BootSettings {
                    order: settings.order.into_iter().map(Into::into).collect(),
                }),
            );
        }

        #[cfg(not(feature = "omicron-build"))]
        if let Some(migration_failure) = migration_failure {
            insert_component(
                &mut spec,
                migration_failure.id,
                ComponentV0::MigrationFailureInjector(migration_failure.spec),
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
                    ComponentV0::SoftNpuPciPort(softnpu_pci),
                );
            }

            if let Some(p9) = softnpu.p9_device {
                insert_component(
                    &mut spec,
                    SpecKey::Name(format!("softnpu-p9-{}", p9.pci_path)),
                    ComponentV0::SoftNpuP9(p9),
                );
            }

            if let Some(p9fs) = softnpu.p9fs {
                insert_component(
                    &mut spec,
                    SpecKey::Name(format!("p9fs-{}", p9fs.pci_path)),
                    ComponentV0::P9fs(p9fs),
                );
            }

            for (port_id, port) in softnpu.ports {
                insert_component(
                    &mut spec,
                    port_id.clone(),
                    ComponentV0::SoftNpuPort(SoftNpuPortSpec {
                        id: port_id,
                        backend_id: port.backend_id.clone(),
                    }),
                );

                insert_component(
                    &mut spec,
                    port.backend_id,
                    ComponentV0::DlpiNetworkBackend(port.backend_spec),
                );
            }
        }

        spec
    }
}

impl TryFrom<InstanceSpecV0> for Spec {
    type Error = ApiSpecError;

    fn try_from(value: InstanceSpecV0) -> Result<Self, Self::Error> {
        let mut builder = SpecBuilder::with_instance_spec_board(value.board)?;
        let mut devices: Vec<(SpecKey, ComponentV0)> = vec![];
        let mut boot_settings = None;
        let mut storage_backends: HashMap<SpecKey, StorageBackend> =
            HashMap::new();
        let mut viona_backends: HashMap<SpecKey, VirtioNetworkBackend> =
            HashMap::new();
        let mut dlpi_backends: HashMap<SpecKey, DlpiNetworkBackend> =
            HashMap::new();

        for (id, component) in value.components.into_iter() {
            match component {
                ComponentV0::CrucibleStorageBackend(_)
                | ComponentV0::FileStorageBackend(_)
                | ComponentV0::BlobStorageBackend(_) => {
                    storage_backends.insert(
                        id,
                        component.try_into().expect(
                            "component is known to be a storage backend",
                        ),
                    );
                }
                ComponentV0::VirtioNetworkBackend(viona) => {
                    viona_backends.insert(id, viona);
                }
                ComponentV0::DlpiNetworkBackend(dlpi) => {
                    dlpi_backends.insert(id, dlpi);
                }
                device => {
                    devices.push((id, device));
                }
            }
        }

        for (device_id, device_spec) in devices {
            match device_spec {
                ComponentV0::VirtioDisk(_) | ComponentV0::NvmeDisk(_) => {
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
                ComponentV0::VirtioNic(nic) => {
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
                ComponentV0::SerialPort(port) => {
                    builder.add_serial_port(device_id, port.num)?;
                }
                ComponentV0::PciPciBridge(bridge) => {
                    builder.add_pci_bridge(device_id, bridge)?;
                }
                ComponentV0::QemuPvpanic(pvpanic) => {
                    builder.add_pvpanic_device(QemuPvpanic {
                        id: device_id,
                        spec: pvpanic,
                    })?;
                }
                ComponentV0::BootSettings(settings) => {
                    // The builder returns an error if its caller tries to add
                    // a boot option that isn't in the set of attached disks.
                    // Since there may be more disk devices left in the
                    // component map, just capture the boot order for now and
                    // apply it to the builder later.
                    boot_settings = Some((device_id, settings));
                }
                #[cfg(not(feature = "falcon"))]
                ComponentV0::SoftNpuPciPort(_)
                | ComponentV0::SoftNpuPort(_)
                | ComponentV0::SoftNpuP9(_)
                | ComponentV0::P9fs(_) => {
                    return Err(ApiSpecError::FeatureCompiledOut(device_id));
                }
                #[cfg(feature = "falcon")]
                ComponentV0::SoftNpuPciPort(port) => {
                    builder.set_softnpu_pci_port(port)?;
                }
                #[cfg(feature = "falcon")]
                ComponentV0::SoftNpuPort(port) => {
                    let (_, backend_spec) = dlpi_backends
                        .remove_entry(&port.backend_id)
                        .ok_or_else(|| {
                            ApiSpecError::NetworkBackendNotFound {
                                backend: port.backend_id.clone(),
                                device: device_id.clone(),
                            }
                        })?;

                    let port = SoftNpuPort {
                        backend_id: port.backend_id,
                        backend_spec,
                    };

                    builder.add_softnpu_port(device_id, port)?;
                }
                #[cfg(feature = "falcon")]
                ComponentV0::SoftNpuP9(p9) => {
                    builder.set_softnpu_p9(p9)?;
                }
                #[cfg(feature = "falcon")]
                ComponentV0::P9fs(p9fs) => {
                    builder.set_p9fs(p9fs)?;
                }

                #[cfg(not(feature = "omicron-build"))]
                ComponentV0::MigrationFailureInjector(injector) => {
                    builder.add_migration_failure_device(
                        super::MigrationFailure {
                            id: device_id,
                            spec: injector,
                        },
                    )?;
                }

                #[cfg(feature = "omicron-build")]
                ComponentV0::MigrationFailureInjector(_) => {
                    return Err(ApiSpecError::FeatureCompiledOut(device_id));
                }

                ComponentV0::CrucibleStorageBackend(_)
                | ComponentV0::FileStorageBackend(_)
                | ComponentV0::BlobStorageBackend(_)
                | ComponentV0::VirtioNetworkBackend(_)
                | ComponentV0::DlpiNetworkBackend(_) => {
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
