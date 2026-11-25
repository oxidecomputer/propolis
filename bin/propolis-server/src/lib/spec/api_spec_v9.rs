// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Conversions between [`propolis_api_types_versions::v9`] instance specs and
//! the internal [`super::Spec`] representation.

use std::collections::BTreeMap;

use propolis_api_types::instance_spec::SpecKey;
use propolis_api_types_versions::{v6, v7, v9};

use super::{builder::SpecBuilder, ApiSpecError, Spec};
use crate::migrate::MigrateError;
use crate::spec::{api_spec_latest, api_spec_v6};

impl From<Spec> for v9::instance_spec::InstanceSpec {
    fn from(mut spec: Spec) -> Self {
        let (v6_spec, topo) = {
            // v9 adds a CpuTopology field to the instance's Board. Remove it,
            // convert the rest, add it back.
            let older_topo =
                v9::instance_spec::CpuTopology::legacy_single_thread_sockets(
                    spec.board.cpus,
                );
            let topo =
                std::mem::replace(&mut spec.board.cpu_topology, older_topo);
            let v6_spec: v6::instance_spec::InstanceSpec =
                spec.try_into().unwrap_or_else(|e| {
                    unreachable!(
                        "Converting to Spec without v9 bits to v6 failed: {e} \
                     This is currently impossible. When spec to \
                     v9::instance_spec::InstanceSpec becomes fallible, \
                     this should `?`."
                    );
                });
            (v6_spec, topo)
        };

        let mut spec: v9::instance_spec::InstanceSpec = v6_spec.into();
        spec.board.cpu_topology = topo;

        spec
    }
}

/// Parses a v9 instance spec into a [`SpecBuilder`], validating component
/// names, PCI paths, and backend references along the way. Callers can add
/// additional (non-v9) components to the builder before calling `finish()`.
pub(crate) fn v9_to_spec_builder(
    value: v9::instance_spec::InstanceSpec,
) -> Result<SpecBuilder, ApiSpecError> {
    api_spec_latest::latest_to_spec_builder(value)
}

pub(crate) fn amend(
    spec: &mut v9::instance_spec::InstanceSpec,
    replacements: &BTreeMap<SpecKey, v7::instance::ReplacementComponent>,
) -> Result<(), MigrateError> {
    for (id, replacement) in replacements {
        let Some(to_amend) = spec.components.get_mut(id) else {
            return Err(MigrateError::InstanceSpecsIncompatible(format!(
                "replacement component {id} not in source spec",
            )));
        };

        // v9 only changes `Board`, so the components which need amending are still the old v6
        // forms. That means we can just reuse the corresponding v6 `amend_component()`..
        api_spec_v6::amend_component(id, to_amend, replacement)?;
    }

    Ok(())
}

#[cfg(test)]
mod test {
    use propolis_api_types_versions::latest::components::board::{
        Board, Chipset, CpuTopology, Cpuid, GuestHypervisorInterface, I440Fx,
    };
    use propolis_api_types_versions::latest::{
        self,
        instance_spec::{CpuidVendor, SmbiosType1Input},
    };
    use propolis_api_types_versions::{v6, v9};

    #[test]
    fn cpu_topology() {
        let api_spec = latest::instance_spec::InstanceSpec {
            board: Board {
                cpus: 4,
                memory_mb: 512,
                chipset: Chipset::I440Fx(I440Fx { enable_pcie: false }),
                guest_hv_interface: GuestHypervisorInterface::Bhyve,
                // Providing *any* CPUID settings keeps Propolis from querying
                // bhyve for defaults to use instead, which requires .. byhve
                // *and* VMM access. Provide a (useless) empty CPUID set so this
                // test can run on non-illumos test systems.
                cpuid: Some(Cpuid {
                    entries: vec![],
                    vendor: CpuidVendor::Amd,
                }),
                cpu_topology: CpuTopology::single_socket_smt(),
            },
            components: Default::default(),
            smbios: Some(SmbiosType1Input {
                manufacturer: "a4x2".to_string(),
                product_name: "913-0000019".to_string(),
                serial_number: "2FAKE000".to_string(),
                version: 2,
            }),
        };

        let test_topo = api_spec.board.cpu_topology;

        let spec =
            crate::spec::api_spec_latest::latest_to_spec_builder(api_spec)
                .unwrap()
                .finish();

        let v9_spec = v9::instance_spec::InstanceSpec::from(spec.clone());
        assert_eq!(v9_spec.board.cpu_topology, test_topo);

        let v6_res = v6::instance_spec::InstanceSpec::try_from(spec.clone());
        assert!(v6_res.is_err());
    }
}
