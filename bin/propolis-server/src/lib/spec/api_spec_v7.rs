// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Conversions between [`propolis_api_types_versions::v7`] instance specs and
//! the internal [`super::Spec`] representation.

use std::collections::BTreeMap;

use propolis_api_types::instance_spec::SpecKey;
use propolis_api_types_versions::{v1::instance::ReplacementComponent, v7};

use super::{builder::SpecBuilder, ApiSpecError, Spec};
use crate::migrate::MigrateError;
use crate::spec::{api_spec_latest, api_spec_v6};

impl From<Spec> for v7::instance_spec::InstanceSpec {
    fn from(mut spec: Spec) -> Self {
        let (v6_spec, topo) = {
            // v7 adds a CpuTopology field to the instance's Board. Remove it,
            // convert the rest, add it back.
            let older_topo =
                v7::instance_spec::CpuTopology::legacy_single_thread_sockets(
                    spec.board.cpus,
                );
            let topo =
                std::mem::replace(&mut spec.board.cpu_topology, older_topo);
            (spec, topo)
        };

        let mut spec: v7::instance_spec::InstanceSpec = v6_spec.into();
        spec.board.cpu_topology = topo;

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

        // v7 only changes `Board`, so the components which need amending are still the old v6
        // forms. That means we can just reuse the corresponding v6 `amend_component()`..
        api_spec_v6::amend_component(id, to_amend, replacement)?;
    }

    Ok(())
}
