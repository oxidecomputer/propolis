// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Conversions between [`propolis_api_types_versions::v6`] instance specs and
//! the internal [`super::Spec`] representation.

use std::collections::BTreeMap;

use propolis_api_types::instance_spec::SpecKey;
use propolis_api_types_versions::{
    v1::instance::ReplacementComponent, v3, v6, v7,
};

use super::{
    builder::SpecBuilder, ApiSpecError, Disk, LegacyApiSpecError, Spec,
    StorageDevice,
};
use crate::migrate::MigrateError;
use crate::spec::api_spec_latest;

impl TryFrom<Spec> for v6::instance_spec::InstanceSpec {
    type Error = LegacyApiSpecError;

    fn try_from(mut val: Spec) -> Result<Self, Self::Error> {
        // v6 adds a new field on NvmeDisk. Such disks probably can't be
        // converted to v3 components and would cause a conversion from
        // Spec->v3::instance_spec::InstanceSpec to fail. So, extract those
        // disks and convert the rest of the Spec to a
        // v3::instance_spec::InstanceSpec. That conversion fails only if the
        // Spec uses post-v6 features (a v7 SMBIOS type 1 input), in which case
        // no v6 spec exists either. If it succeeds, add the disks back in.
        //
        // TODO: could be extract_if once we're on a Rust >= 1.91.0.
        let mut nvme_disks = Vec::new();
        let v6_only_disk =
            |disk: &Disk| matches!(disk.device_spec, StorageDevice::Nvme(_));
        for (key, disk) in val.disks.iter() {
            if v6_only_disk(disk) {
                nvme_disks.push((key.clone(), disk.clone()));
            }
        }
        val.disks.retain(|_, disk| !v6_only_disk(disk));

        let v3_spec: v3::instance_spec::InstanceSpec = val.try_into()?;

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
                key
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

        Ok(spec)
    }
}

/// Parses a v6 instance spec into a [`SpecBuilder`], validating component
/// names, PCI paths, and backend references along the way. Callers can add
/// additional (non-v6) components to the builder before calling `finish()`.
pub(crate) fn v6_to_spec_builder(
    value: v6::instance_spec::InstanceSpec,
) -> Result<SpecBuilder, ApiSpecError> {
    // Converting v6 to v7 is lossless so just do that and piggyback on the
    // latest `InstanceSpec->SpecBuilder`.
    let v7_spec: v7::instance_spec::InstanceSpec = value.into();

    api_spec_latest::latest_to_spec_builder(v7_spec)
}

pub(crate) fn amend_component(
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
