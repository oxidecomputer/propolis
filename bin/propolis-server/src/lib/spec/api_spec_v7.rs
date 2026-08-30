// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Conversions between [`propolis_api_types_versions::v7`] instance specs and
//! the internal [`super::Spec`] representation.

use std::collections::BTreeMap;

use propolis_api_types::instance_spec::SpecKey;
use propolis_api_types_versions::{v1::instance::ReplacementComponent, v6, v7};

use super::{builder::SpecBuilder, ApiSpecError, Spec};
use crate::migrate::MigrateError;
use crate::spec::{api_spec_latest, api_spec_v6};

impl From<Spec> for v7::instance_spec::InstanceSpec {
    fn from(mut val: Spec) -> Self {
        // v7 only widens the SMBIOS type 1 input, which would cause the
        // conversion to a v6 spec to fail if the new fields are set. Set the
        // input aside, convert the rest as v6, and put it back afterwards.
        let smbios = val.smbios_type1_input.take();

        let v6_spec: v6::instance_spec::InstanceSpec =
            val.try_into().unwrap_or_else(|e| {
                unreachable!(
                    "Converting a Spec without v7 bits to v6 failed: {e}. \
                        This is currently impossible. When Spec to \
                        v7::instance_spec::InstanceSpec becomes fallible, \
                        this should `?`."
                );
            });

        let mut spec: v7::instance_spec::InstanceSpec = v6_spec.into();
        spec.smbios = smbios;
        spec
    }
}

/// Parses a v7 instance spec into a [`SpecBuilder`], validating component
/// names, PCI paths, and backend references along the way. Callers can add
/// additional (non-v7) components to the builder before calling `finish()`.
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

        // v7 reuses the v6 component types, so the v6 amendment logic applies.
        api_spec_v6::amend_component(id, to_amend, replacement)?;
    }

    Ok(())
}
