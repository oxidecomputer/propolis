// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Conversions between [`propolis_api_types_versions::v3`] instance specs and
//! the internal [`super::Spec`] representation.

use std::collections::BTreeMap;

use propolis_api_types::instance_spec::SpecKey;
use propolis_api_types_versions::{
    v1::instance::ReplacementComponent, v2, v3, v6,
};

use super::{api_spec_v6, builder::SpecBuilder, LegacyApiSpecError, Spec};
use crate::migrate::MigrateError;

impl TryFrom<Spec> for v3::instance_spec::InstanceSpec {
    type Error = LegacyApiSpecError;

    fn try_from(mut val: Spec) -> Result<Self, Self::Error> {
        // v3 added only the `vsock` component, which is expressed only as the
        // `vsock` field on `Spec` here. Either we can remove it and this is a
        // Spec that can be interpreted as v2, or this spec is not valid as
        // either.
        let vsock = val.vsock.take();

        let v2_instance_spec: v2::instance_spec::InstanceSpec =
            val.try_into()?;
        let mut instance_spec: v3::instance_spec::InstanceSpec =
            v2_instance_spec.into();

        if let Some(vsock) = vsock {
            let existing = instance_spec.components.insert(
                vsock.id.clone(),
                v3::instance_spec::Component::VirtioSocket(vsock.spec),
            );
            assert!(
                existing.is_none(),
                "there was already a component named {} in the spec?!",
                vsock.id
            );
        }

        Ok(instance_spec)
    }
}

/// Parses a v3 instance spec into a [`SpecBuilder`], validating component
/// names, PCI paths, and backend references along the way. Callers can add
/// additional (non-v3) components to the builder before calling `finish()`.
pub(crate) fn v3_to_spec_builder(
    value: v3::instance_spec::InstanceSpec,
) -> Result<SpecBuilder, LegacyApiSpecError> {
    // Converting v3 to v6 is lossless so just do that and piggyback on the
    // v6 `InstanceSpec->SpecBuilder`.
    let v6_spec: v6::instance_spec::InstanceSpec = value.into();

    api_spec_v6::v6_to_spec_builder(v6_spec).map_err(|e| e.into())
}

fn amend_component(
    id: &SpecKey,
    to_amend: &mut v3::instance_spec::Component,
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
            let v3::instance_spec::Component::MigrationFailureInjector(src) =
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
            let v3::instance_spec::Component::CrucibleStorageBackend(src) =
                to_amend
            else {
                return Err(MigrateError::wrong_type(id, "crucible backend"));
            };

            *src = comp.clone();
        }
        ReplacementComponent::VirtioNetworkBackend(comp) => {
            let v3::instance_spec::Component::VirtioNetworkBackend(src) =
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
    spec: &mut v3::instance_spec::InstanceSpec,
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
