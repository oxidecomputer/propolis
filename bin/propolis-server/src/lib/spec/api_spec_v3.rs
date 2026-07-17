// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Conversions from [`propolis_api_types::v3`]) instance specs in the
//! [`propolis_api_types`] crate to the internal [`super::Spec`] representation.

use std::collections::BTreeMap;

use propolis_api_types::instance_spec::{
    components::{
        backends::{DlpiNetworkBackend, VirtioNetworkBackend},
        board::Board as InstanceSpecBoard,
        devices::{BootSettings, SerialPort as SerialPortDesc},
    },
    SpecKey,
};
use propolis_api_types_versions::{v3, latest};
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

use crate::spec::api_spec_v1;
impl From<ApiSpecError> for api_spec_v1::ApiSpecError {
    fn from(value: ApiSpecError) -> Self {
        match value {
            ApiSpecError::Builder(b) => api_spec_v1::ApiSpecError::Builder(b),
            ApiSpecError::StorageBackendNotFound { backend, device } => api_spec_v1::ApiSpecError::StorageBackendNotFound { backend, device },
            ApiSpecError::NetworkBackendNotFound { backend, device } => api_spec_v1::ApiSpecError::NetworkBackendNotFound { backend, device },
            ApiSpecError::FeatureCompiledOut { component, feature } => api_spec_v1::ApiSpecError::FeatureCompiledOut { component, feature },
            ApiSpecError::BackendNotUsed(key) => api_spec_v1::ApiSpecError::BackendNotUsed(key),
        }
    }
}

// TODO: docs. conversion back down from v6 to v3 because we defer InstanceSpec->Spec to
// `v6_to_spec_builder()`.
use crate::spec::api_spec_v6;
impl From<api_spec_v6::ApiSpecError> for ApiSpecError {
    fn from(value: api_spec_v6::ApiSpecError) -> Self {
        match value {
            api_spec_v6::ApiSpecError::Builder(b) => ApiSpecError::Builder(b),
            api_spec_v6::ApiSpecError::StorageBackendNotFound { backend, device } => ApiSpecError::StorageBackendNotFound { backend, device },
            api_spec_v6::ApiSpecError::NetworkBackendNotFound { backend, device } => ApiSpecError::NetworkBackendNotFound { backend, device },
            api_spec_v6::ApiSpecError::FeatureCompiledOut { component, feature } => ApiSpecError::FeatureCompiledOut { component, feature },
            api_spec_v6::ApiSpecError::BackendNotUsed(key) => ApiSpecError::BackendNotUsed(key),
        }
    }
}

impl TryFrom<Spec> for v3::instance_spec::InstanceSpec {
    type Error = String;

    fn try_from(val: Spec) -> Result<Self, Self::Error> {
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
            smbios_type1_input,
            vsock,
            #[cfg(feature = "failure-injection")]
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
            spec: &mut v3::instance_spec::InstanceSpec,
            key: SpecKey,
            val: v3::instance_spec::Component,
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
        let mut spec = v3::instance_spec::InstanceSpec {
            board,
            smbios: smbios_type1_input,
            components: Default::default(),
        };

        for (disk_id, disk) in disks {
            let backend_id = disk.device_spec.backend_id().to_owned();
            let device_component: v3::instance_spec::Component = disk.device_spec.try_into().expect("TODO: StorageDevice into v3::Component");
            let backend_component: v3::instance_spec::Component = disk.backend_spec.into();
            insert_component(&mut spec, disk_id, device_component);
            insert_component(&mut spec, backend_id, backend_component);
        }

        for (nic_id, nic) in nics {
            let backend_id = nic.device_spec.backend_id.clone();
            insert_component(
                &mut spec,
                nic_id,
                v3::instance_spec::Component::VirtioNic(nic.device_spec),
            );

            insert_component(
                &mut spec,
                backend_id,
                v3::instance_spec::Component::VirtioNetworkBackend(
                    nic.backend_spec,
                ),
            );
        }

        for (name, desc) in serial {
            if desc.device == SerialPortDevice::Uart {
                insert_component(
                    &mut spec,
                    name,
                    v3::instance_spec::Component::SerialPort(SerialPortDesc {
                        num: desc.num,
                    }),
                );
            }
        }

        for (bridge_name, bridge) in pci_pci_bridges {
            insert_component(
                &mut spec,
                bridge_name,
                v3::instance_spec::Component::PciPciBridge(bridge),
            );
        }

        if let Some(pvpanic) = pvpanic {
            insert_component(
                &mut spec,
                pvpanic.id,
                v3::instance_spec::Component::QemuPvpanic(pvpanic.spec),
            );
        }

        if let Some(vsock) = vsock {
            insert_component(
                &mut spec,
                vsock.id,
                v3::instance_spec::Component::VirtioSocket(vsock.spec),
            );
        }

        if let Some(settings) = boot_settings {
            insert_component(
                &mut spec,
                settings.name,
                v3::instance_spec::Component::BootSettings(BootSettings {
                    order: settings.order.into_iter().map(Into::into).collect(),
                }),
            );
        }

        #[cfg(feature = "failure-injection")]
        if let Some(mig) = migration_failure {
            insert_component(
                &mut spec,
                mig.id,
                v3::instance_spec::Component::MigrationFailureInjector(
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
                    v3::instance_spec::Component::SoftNpuPciPort(softnpu_pci),
                );
            }

            if let Some(p9) = softnpu.p9_device {
                insert_component(
                    &mut spec,
                    SpecKey::Name(format!("softnpu-p9-{}", p9.pci_path)),
                    v3::instance_spec::Component::SoftNpuP9(p9),
                );
            }

            if let Some(p9fs) = softnpu.p9fs {
                insert_component(
                    &mut spec,
                    SpecKey::Name(format!("p9fs-{}", p9fs.pci_path)),
                    v3::instance_spec::Component::P9fs(p9fs),
                );
            }

            for (port_name, port) in softnpu.ports {
                insert_component(
                    &mut spec,
                    port_name.clone(),
                    v3::instance_spec::Component::SoftNpuPort(
                        SoftNpuPortSpec {
                            link_name: port.link_name,
                            backend_id: port.backend_name.clone(),
                        },
                    ),
                );

                insert_component(
                    &mut spec,
                    port.backend_name,
                    v3::instance_spec::Component::DlpiNetworkBackend(
                        port.backend_spec,
                    ),
                );
            }
        }

        Ok(spec)
    }
}

/*
impl TryFrom<v3::instance_spec::InstanceSpec> for Spec {
    type Error = ApiSpecError;

    fn try_from(
        value: v3::instance_spec::InstanceSpec,
    ) -> Result<Self, Self::Error> {
        Ok(v3_to_spec_builder(value)?.finish())
    }
}
*/

/// Parses a v3 instance spec into a [`SpecBuilder`], validating component
/// names, PCI paths, and backend references along the way. Callers can add
/// additional (non-v3) components to the builder before calling `finish()`.
pub(crate) fn v3_to_spec_builder(
    value: v3::instance_spec::InstanceSpec,
) -> Result<SpecBuilder, ApiSpecError> {
    let latest_spec: latest::instance_spec::InstanceSpec = value.into();

    // TODO: talk about this more
    crate::spec::api_spec_v6::latest_api_spec_to_spec_builder(latest_spec)
        .map_err(|e| e.into())
}
