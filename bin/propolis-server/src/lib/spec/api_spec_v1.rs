// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Conversions from the initial API version ([`propolis_api_types::v1`], aka
//! "V0" in some parts of propolis-server) instance specs in the
//! [`propolis_api_types`] crate to the internal [`super::Spec`] representation.

use std::collections::BTreeMap;

use propolis_api_types::instance_spec::{
    components::{
        board::Board as InstanceSpecBoard,
        devices::{BootSettings, SerialPort as SerialPortDesc},
    },
    SpecKey,
};
use propolis_api_types_versions::{
    v1, v1::instance::ReplacementComponent, v2, v3, v6,
};
use thiserror::Error;

#[cfg(feature = "falcon")]
use propolis_api_types::instance_spec::components::devices::SoftNpuPort as SoftNpuPortSpec;

use super::{
    builder::{SpecBuilder, SpecBuilderError},
    SerialPortDevice, Spec, StorageBackend, StorageDevice,
};
use crate::migrate::MigrateError;

#[cfg(feature = "failure-injection")]
use super::MigrationFailure;

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

    #[error("spec contains v1-incompatible component: {0}")]
    IncompatibleComponent(String),
}

// Woah! It's strange to have a conversion to a *v1* type which has an error
// from *v6* about *v3*. Not as bad as it seems though: v6 is when this
// component changed, and v3 is next-most-recent version of
// `instance_spec::Component`. So in v6, the error is "I can't convert this to
// v3::instance_spec::Component".
//
// We're converting to v1, though, which has a further-different `Component`
// type. If we had a fallible operation converting a storage device all the way
// down, that might produce an error about failing to convert a V3 type to V1 -
// it turns out that's infallible here.
impl TryFrom<StorageDevice> for v1::instance_spec::Component {
    type Error = v6::instance_spec::InvalidV3Component;

    fn try_from(value: StorageDevice) -> Result<Self, Self::Error> {
        match value {
            StorageDevice::Virtio(d) => Ok(Self::VirtioDisk(d)),
            StorageDevice::Nvme(d) => Ok(Self::NvmeDisk(d.try_into()?)),
        }
    }
}

impl From<StorageBackend> for v1::instance_spec::Component {
    fn from(value: StorageBackend) -> Self {
        match value {
            StorageBackend::Crucible(be) => Self::CrucibleStorageBackend(be),
            StorageBackend::File(be) => Self::FileStorageBackend(be),
            StorageBackend::Blob(be) => Self::BlobStorageBackend(be),
        }
    }
}

impl TryFrom<Spec> for v1::instance_spec::InstanceSpec {
    type Error = ApiSpecError;

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
            #[cfg(feature = "failure-injection")]
            migration_failure,
            #[cfg(feature = "falcon")]
            softnpu,

            // Not part of `v1::instance_spec::InstanceSpec`. Added in
            // `InstanceSpec` in API Version 2.0.0.
            smbios_type1_input,

            // Not part of `v1::instance_spec::InstanceSpec`. Added in
            // `InstanceSpec` in API Version 3.0.0.
            vsock,
        } = val;

        if smbios_type1_input.is_some() {
            // NOTE: This is overly strict. There is one specific SMBIOS Type 1
            // table that could be expressed previously, and that is the one
            // where the instance serial is set to the instance UUID.
            //
            // This is the Type 1 table provided by Nexus as of specs later than
            // V1, so by bailing here we're effectively blocking migration from
            // new Propolises to old Propolises. This is acceptable for a few
            // reasons:
            // * the control plane is not expected to migrate VMs to down-rev
            //   Propolises
            // * V1 specs are from before live migration was done outside
            //   ad-hoc/CI environments - such an old Propolis will never exist
            //   as a migration target in the field.
            return Err(ApiSpecError::IncompatibleComponent(
                "cannot express explicit SMBIOS tables in v1 instance spec"
                    .to_string(),
            ));
        }

        if vsock.is_some() {
            return Err(ApiSpecError::IncompatibleComponent(
                "cannot convert virtio-socket to v1 instance spec".to_string(),
            ));
        }

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
            let device_component: v1::instance_spec::Component = disk.device_spec.try_into()
                .map_err(|e: propolis_api_types_versions::v6::instance_spec::InvalidV3Component| ApiSpecError::IncompatibleComponent(e.to_string()))?;
            let backend_component: v1::instance_spec::Component =
                disk.backend_spec.into();
            insert_component(&mut spec, disk_id, device_component);
            insert_component(&mut spec, backend_id, backend_component);
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

        Ok(spec)
    }
}

impl TryFrom<v1::instance_spec::InstanceSpec> for Spec {
    type Error = ApiSpecError;

    fn try_from(
        value: v1::instance_spec::InstanceSpec,
    ) -> Result<Self, Self::Error> {
        Ok(v1_to_spec_builder(value)?.finish())
    }
}

/// Parses a v1 instance spec into a [`SpecBuilder`], validating component
/// names, PCI paths, and backend references along the way. Callers can add
/// additional (non-v1) components to the builder before calling `finish()`.
pub(crate) fn v1_to_spec_builder(
    value: v1::instance_spec::InstanceSpec,
) -> Result<SpecBuilder, ApiSpecError> {
    let v2_spec: v2::instance_spec::InstanceSpec = value.into();
    let v3_spec: v3::instance_spec::InstanceSpec = v2_spec.into();

    crate::spec::api_spec_v3::v3_to_spec_builder(v3_spec).map_err(|e| e.into())
}

// `amend_component` is suitable for (and used in) amending a v2 InstanceSpec,
// so this one is pub(crate) unlike other `api_spec_v*`.
pub(crate) fn amend_component(
    id: &SpecKey,
    to_amend: &mut v1::instance_spec::Component,
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
            let v1::instance_spec::Component::MigrationFailureInjector(src) =
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
            let v1::instance_spec::Component::CrucibleStorageBackend(src) =
                to_amend
            else {
                return Err(MigrateError::wrong_type(id, "crucible backend"));
            };

            *src = comp.clone();
        }
        ReplacementComponent::VirtioNetworkBackend(comp) => {
            let v1::instance_spec::Component::VirtioNetworkBackend(src) =
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
    spec: &mut v1::instance_spec::InstanceSpec,
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
