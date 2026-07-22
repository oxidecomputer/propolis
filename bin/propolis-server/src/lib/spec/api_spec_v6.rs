// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Conversions from [`propolis_api_types::v6`] instance specs in the
//! [`propolis_api_types`] crate to the internal [`super::Spec`] representation.

use std::collections::BTreeMap;

use propolis_api_types::instance_spec::{
    components::backends::{DlpiNetworkBackend, VirtioNetworkBackend},
    SpecKey,
};
use propolis_api_types_versions::{v1::instance::ReplacementComponent, v3, v6};
use thiserror::Error;

use super::{
    builder::{SpecBuilder, SpecBuilderError},
    Disk, Nic, QemuPvpanic, Spec, StorageBackend, StorageDevice,
};
use crate::migrate::MigrateError;

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

impl From<Spec> for v6::instance_spec::InstanceSpec {
    fn from(mut val: Spec) -> Self {
        // v6 adds a new field on NvmeDisk. Such disks probably can't be
        // converted to v3 components and would cause a conversion from
        // Spec->v3::instance_spec::InstanceSpec to fail. So, extract those
        // disks and convert the rest of the Spec to a
        // v3::instance_spec::InstanceSpec. If this fails, we wouldn't have been
        // able to get a v6 spec anyway. If it succeeds, we can add the disks
        // back in here.
        //
        // TODO: could be extract_if once we're on a Rust >= 1.91.0.
        let mut nvme_disks = Vec::new();
        for (key, disk) in val.disks.iter() {
            let should_remove = match disk.device_spec {
                StorageDevice::Nvme(_) => true,
                _ => false,
            };
            if should_remove {
                nvme_disks.push((key.clone(), disk.clone()));
            }
        }
        val.disks.retain(|_, disk| match disk.device_spec {
            StorageDevice::Nvme(_) => false,
            _ => true,
        });

        let v3_spec: v3::instance_spec::InstanceSpec =
            val.try_into().unwrap_or_else(|e| {
                unreachable!(
                    "Converting to Spec without v6 bits to v3 failed: {e}. \
                        This is currently impossible. When Spec to \
                        v6::instance_spec::InstanceSpec becomes fallible, \
                        this should `?`."
                );
            });

        let mut spec: v6::instance_spec::InstanceSpec = v3_spec.into();

        // Inserts a component entry into the supplied map, asserting first that
        // the supplied key is not present in that map.
        //
        // This assertion is valid because internal instance specs should assign
        // a unique name to each component they describe. The spec builder
        // upholds this invariant at spec creation time.
        #[track_caller]
        fn insert_component(
            spec: &mut v6::instance_spec::InstanceSpec,
            key: SpecKey,
            val: v6::instance_spec::Component,
        ) {
            assert!(
                !spec.components.contains_key(&key),
                "component name {} already exists in output spec",
                &key
            );
            spec.components.insert(key, val);
        }

        for (disk_id, disk) in nvme_disks {
            let backend_id = disk.device_spec.backend_id().to_owned();
            let device_component: v6::instance_spec::Component =
                disk.device_spec.into();
            let backend_component: v6::instance_spec::Component =
                disk.backend_spec.into();
            insert_component(&mut spec, disk_id, device_component);
            insert_component(&mut spec, backend_id, backend_component);
        }

        spec
    }
}

/// Parses a v6 instance spec into a [`SpecBuilder`], validating component
/// names, PCI paths, and backend references along the way. Callers can add
/// additional (non-v6) components to the builder before calling `finish()`.
pub(crate) fn v6_to_spec_builder(
    value: v6::instance_spec::InstanceSpec,
) -> Result<SpecBuilder, ApiSpecError> {
    let mut builder = SpecBuilder::with_instance_spec_board(value.board)?;
    let mut devices: Vec<(SpecKey, v6::instance_spec::Component)> = vec![];
    let mut boot_settings = None;
    let mut storage_backends: BTreeMap<SpecKey, StorageBackend> =
        BTreeMap::new();
    let mut viona_backends: BTreeMap<SpecKey, VirtioNetworkBackend> =
        BTreeMap::new();
    let mut dlpi_backends: BTreeMap<SpecKey, DlpiNetworkBackend> =
        BTreeMap::new();

    for (id, component) in value.components.into_iter() {
        match component {
            v6::instance_spec::Component::CrucibleStorageBackend(_)
            | v6::instance_spec::Component::FileStorageBackend(_)
            | v6::instance_spec::Component::BlobStorageBackend(_) => {
                storage_backends.insert(
                    id,
                    component
                        .try_into()
                        .expect("component is known to be a storage backend"),
                );
            }
            v6::instance_spec::Component::VirtioNetworkBackend(viona) => {
                viona_backends.insert(id, viona);
            }
            v6::instance_spec::Component::DlpiNetworkBackend(dlpi) => {
                dlpi_backends.insert(id, dlpi);
            }
            device => {
                devices.push((id, device));
            }
        }
    }

    for (device_id, device_spec) in devices {
        match device_spec {
            v6::instance_spec::Component::VirtioDisk(_)
            | v6::instance_spec::Component::NvmeDisk(_) => {
                let device_spec = StorageDevice::try_from(device_spec)
                    .expect("component is known to be a disk");

                let (_, backend_spec) = storage_backends
                    .remove_entry(device_spec.backend_id())
                    .ok_or_else(|| ApiSpecError::StorageBackendNotFound {
                        backend: device_spec.backend_id().to_owned(),
                        device: device_id.clone(),
                    })?;

                builder.add_storage_device(
                    device_id,
                    Disk { device_spec, backend_spec },
                )?;
            }
            v6::instance_spec::Component::VirtioNic(nic) => {
                let (_, backend_spec) = viona_backends
                    .remove_entry(&nic.backend_id)
                    .ok_or_else(|| ApiSpecError::NetworkBackendNotFound {
                        backend: nic.backend_id.clone(),
                        device: device_id.clone(),
                    })?;

                builder.add_network_device(
                    device_id,
                    Nic { device_spec: nic, backend_spec },
                )?;
            }
            v6::instance_spec::Component::SerialPort(port) => {
                builder.add_serial_port(device_id, port.num)?;
            }
            v6::instance_spec::Component::PciPciBridge(bridge) => {
                builder.add_pci_bridge(device_id, bridge)?;
            }
            v6::instance_spec::Component::QemuPvpanic(pvpanic) => {
                builder.add_pvpanic_device(QemuPvpanic {
                    id: device_id,
                    spec: pvpanic,
                })?;
            }
            v6::instance_spec::Component::BootSettings(settings) => {
                // The builder returns an error if its caller tries to add
                // a boot option that isn't in the set of attached disks.
                // Since there may be more disk devices left in the
                // component map, just capture the boot order for now and
                // apply it to the builder later.
                boot_settings = Some((device_id, settings));
            }
            v6::instance_spec::Component::VirtioSocket(vsock) => {
                let vsock_device = crate::spec::VirtioSocket {
                    id: device_id.clone(),
                    spec: vsock,
                };
                builder.add_vsock_device(vsock_device)?;
            }
            #[cfg(not(feature = "failure-injection"))]
            v6::instance_spec::Component::MigrationFailureInjector(_) => {
                return Err(ApiSpecError::FeatureCompiledOut {
                    component: device_id,
                    feature: "failure-injection",
                });
            }
            #[cfg(feature = "failure-injection")]
            v6::instance_spec::Component::MigrationFailureInjector(mig) => {
                builder.add_migration_failure_device(MigrationFailure {
                    id: device_id,
                    spec: mig,
                })?;
            }
            #[cfg(not(feature = "falcon"))]
            v6::instance_spec::Component::SoftNpuPciPort(_)
            | v6::instance_spec::Component::SoftNpuPort(_)
            | v6::instance_spec::Component::SoftNpuP9(_)
            | v6::instance_spec::Component::P9fs(_) => {
                return Err(ApiSpecError::FeatureCompiledOut {
                    component: device_id,
                    feature: "falcon",
                });
            }
            #[cfg(feature = "falcon")]
            v6::instance_spec::Component::SoftNpuPciPort(port) => {
                builder.set_softnpu_pci_port(port)?;
            }
            #[cfg(feature = "falcon")]
            v6::instance_spec::Component::SoftNpuPort(port) => {
                let (_, backend_spec) = dlpi_backends
                    .remove_entry(&port.backend_id)
                    .ok_or_else(|| ApiSpecError::NetworkBackendNotFound {
                        backend: port.backend_id.clone(),
                        device: device_id.clone(),
                    })?;

                let port = SoftNpuPort {
                    link_name: port.link_name,
                    backend_name: port.backend_id,
                    backend_spec,
                };

                builder.add_softnpu_port(device_id, port)?;
            }
            #[cfg(feature = "falcon")]
            v6::instance_spec::Component::SoftNpuP9(p9) => {
                builder.set_softnpu_p9(p9)?;
            }
            #[cfg(feature = "falcon")]
            v6::instance_spec::Component::P9fs(p9fs) => {
                builder.set_p9fs(p9fs)?;
            }
            v6::instance_spec::Component::CrucibleStorageBackend(_)
            | v6::instance_spec::Component::FileStorageBackend(_)
            | v6::instance_spec::Component::BlobStorageBackend(_)
            | v6::instance_spec::Component::VirtioNetworkBackend(_)
            | v6::instance_spec::Component::DlpiNetworkBackend(_) => {
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

    Ok(builder)
}

fn amend_component(
    id: &SpecKey,
    to_amend: &mut v6::instance_spec::Component,
    replacement: &ReplacementComponent,
) -> Result<(), MigrateError> {
    match replacement {
        #[cfg(not(feature = "failure-injection"))]
        ReplacementComponent::MigrationFailureInjector(_) => {
            return Err(MigrateError::InstanceSpecsIncompatible(format!(
                "replacing migration failure injector {id} is \
                    impossible because the feature is compiled out"
            )));
        }

        #[cfg(feature = "failure-injection")]
        ReplacementComponent::MigrationFailureInjector(comp) => {
            let v6::instance_spec::Component::MigrationFailureInjector(src) =
                to_amend
            else {
                return Err(MigrateError::wrong_type(
                    id,
                    "migration failure injector",
                ));
            };

            *src = comp.clone();
        }
        ReplacementComponent::CrucibleStorageBackend(comp) => {
            let v6::instance_spec::Component::CrucibleStorageBackend(src) =
                to_amend
            else {
                return Err(MigrateError::wrong_type(id, "crucible backend"));
            };

            *src = comp.clone();
        }
        ReplacementComponent::VirtioNetworkBackend(comp) => {
            let v6::instance_spec::Component::VirtioNetworkBackend(src) =
                to_amend
            else {
                return Err(MigrateError::wrong_type(id, "viona backend"));
            };

            *src = comp.clone();
        }
    }

    Ok(())
}

pub(crate) fn amend(
    spec: &mut v6::instance_spec::InstanceSpec,
    replacements: &BTreeMap<SpecKey, ReplacementComponent>,
) -> Result<(), MigrateError> {
    for (id, replacement) in replacements {
        let Some(to_amend) = spec.components.get_mut(id) else {
            return Err(MigrateError::InstanceSpecsIncompatible(format!(
                "replacement component {id} not in source spec",
            )));
        };

        amend_component(id, to_amend, replacement)?;
    }

    Ok(())
}
