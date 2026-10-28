// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Conversions between [`propolis_api_types_versions::v7`] instance specs and
//! the internal [`super::Spec`] representation.

use std::collections::BTreeMap;

use propolis_api_types::instance_spec::SpecKey;
use propolis_api_types_versions::{v1::instance::ReplacementComponent, v6, v7};

use super::{
    builder::SpecBuilder, ApiSpecError, LegacyApiSpecError, Spec,
    StorageBackend, StorageDevice,
};
use crate::migrate::MigrateError;
use crate::spec::api_spec_latest;

impl From<StorageDevice> for v7::instance_spec::Component {
    fn from(value: StorageDevice) -> Self {
        match value {
            StorageDevice::Virtio(d) => Self::VirtioDisk(d),
            StorageDevice::Nvme(d) => Self::NvmeDisk(d),
        }
    }
}

impl From<StorageBackend> for v7::instance_spec::Component {
    fn from(value: StorageBackend) -> Self {
        match value {
            StorageBackend::Crucible(b) => Self::CrucibleStorageBackend(b),
            StorageBackend::File(b) => Self::FileStorageBackend(b),
            StorageBackend::Blob(b) => Self::BlobStorageBackend(b),
        }
    }
}

impl TryFrom<Spec> for v7::instance_spec::InstanceSpec {
    type Error = LegacyApiSpecError;

    fn try_from(mut val: Spec) -> Result<Self, Self::Error> {
        // v7 added only the `acpi` component, which is expressed only as the
        // `acpi` field on `Spec` here. Either we can remove it and this is a
        // Spec that can be interpreted as v6, or this spec is not valid as v7
        // either.
        let acpi = val.acpi.take();

        let v6_spec: v6::instance_spec::InstanceSpec = val.try_into()?;
        let mut spec: v7::instance_spec::InstanceSpec = v6_spec.into();

        if let Some(acpi) = acpi {
            let existing = spec.components.insert(
                acpi.id.clone(),
                v7::instance_spec::Component::Acpi(acpi.spec),
            );
            assert!(
                existing.is_none(),
                "there was already a component named {} in the spec?!",
                acpi.id
            );
        }

        Ok(spec)
    }
}

/// Parses a v7 instance spec into a [`SpecBuilder`], validating component
/// names, PCI paths, and backend references along the way. Callers can add
/// additional (non-v7) components to the builder before calling `finish()`.
#[allow(clippy::useless_conversion)]
pub(crate) fn v7_to_spec_builder(
    value: v7::instance_spec::InstanceSpec,
) -> Result<SpecBuilder, ApiSpecError> {
    api_spec_latest::latest_to_spec_builder(value)
}

pub(crate) fn amend(
    spec: &mut v7::instance_spec::InstanceSpec,
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

fn amend_component(
    id: &SpecKey,
    to_amend: &mut v7::instance_spec::Component,
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
            let v7::instance_spec::Component::MigrationFailureInjector(src) =
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
            let v7::instance_spec::Component::CrucibleStorageBackend(src) =
                to_amend
            else {
                return Err(MigrateError::wrong_type(id, "crucible backend"));
            };

            *src = comp.clone();
        }
        ReplacementComponent::VirtioNetworkBackend(comp) => {
            let v7::instance_spec::Component::VirtioNetworkBackend(src) =
                to_amend
            else {
                return Err(MigrateError::wrong_type(id, "viona backend"));
            };

            *src = comp.clone();
        }
    }

    Ok(())
}
