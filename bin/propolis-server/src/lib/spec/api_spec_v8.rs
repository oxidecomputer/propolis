// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Conversions between [`propolis_api_types_versions::v8`] instance specs and
//! the internal [`super::Spec`] representation.

use std::collections::BTreeMap;

use propolis_api_types::instance::ReplacementComponent;
use propolis_api_types::instance_spec::SpecKey;
use propolis_api_types_versions::{v6, v8};

use super::{api_spec_latest, builder::SpecBuilder, ApiSpecError, Spec};
use crate::migrate::MigrateError;

impl From<Spec> for v8::instance_spec::InstanceSpec {
    fn from(mut val: Spec) -> Self {
        // v8 only changes the guest hypervisor interface, allowing it to carry
        // features that can't be expressed in v6. Swap in the default
        // interface (which every version can express), convert the rest of
        // the spec to v6, and then put the real interface back.
        let guest_hv_interface =
            std::mem::take(&mut val.board.guest_hv_interface);

        let v6_spec: v6::instance_spec::InstanceSpec =
            val.try_into().unwrap_or_else(|e| {
                unreachable!(
                    "Converting Spec without v8 bits to v6 failed: {e}. \
                        This is currently impossible. When Spec to \
                        v8::instance_spec::InstanceSpec becomes fallible, \
                        this should `?`."
                );
            });

        let mut spec: v8::instance_spec::InstanceSpec = v6_spec.into();
        spec.board.guest_hv_interface = guest_hv_interface;
        spec
    }
}

/// Parses a v8 instance spec into a [`SpecBuilder`], validating component
/// names, PCI paths, and backend references along the way. Callers can add
/// additional (non-v8) components to the builder before calling `finish()`.
pub(crate) fn v8_to_spec_builder(
    value: v8::instance_spec::InstanceSpec,
) -> Result<SpecBuilder, ApiSpecError> {
    api_spec_latest::latest_to_spec_builder(value)
}

pub(crate) fn amend(
    spec: &mut v8::instance_spec::InstanceSpec,
    replacements: &BTreeMap<SpecKey, ReplacementComponent>,
) -> Result<(), MigrateError> {
    for (id, replacement) in replacements {
        let Some(to_amend) = spec.components.get_mut(id) else {
            return Err(MigrateError::InstanceSpecsIncompatible(format!(
                "replacement component {id} not in source spec",
            )));
        };

        super::api_spec_v6::amend_component(id, to_amend, replacement)?;
    }

    Ok(())
}

#[cfg(test)]
pub(crate) mod test {
    use propolis_api_types_versions::latest::components::board::{
        BhyveFeatureFlag, Board, Chipset, Cpuid, GuestHypervisorInterface,
        HyperVFeatureFlag, I440Fx,
    };
    use propolis_api_types_versions::latest::{
        self, instance_spec::CpuidVendor,
    };
    use propolis_api_types_versions::{v1, v6, v8};

    /// Builds an internal spec with no components and the supplied guest
    /// hypervisor interface.
    pub(crate) fn spec_with_interface(
        guest_hv_interface: GuestHypervisorInterface,
    ) -> crate::spec::Spec {
        let api_spec = latest::instance_spec::InstanceSpec {
            board: Board {
                cpus: 4,
                memory_mb: 512,
                chipset: Chipset::I440Fx(I440Fx { enable_pcie: false }),
                guest_hv_interface,
                // Providing *any* CPUID settings keeps Propolis from querying
                // bhyve for defaults to use instead, which requires bhyve
                // *and* VMM access. Provide a (useless) empty CPUID set so this
                // test can run on non-illumos test systems.
                cpuid: Some(Cpuid {
                    entries: vec![],
                    vendor: CpuidVendor::Amd,
                }),
            },
            components: Default::default(),
            smbios: None,
        };

        crate::spec::api_spec_latest::latest_to_spec_builder(api_spec)
            .unwrap()
            .finish()
    }

    /// Guest hypervisor interfaces that can only be expressed in v8 and later.
    pub(crate) fn v8_only_interfaces(
    ) -> impl IntoIterator<Item = GuestHypervisorInterface> {
        [
            GuestHypervisorInterface::Bhyve {
                features: [BhyveFeatureFlag::ExtMsiDestId].into(),
            },
            GuestHypervisorInterface::HyperV {
                features: [
                    HyperVFeatureFlag::ReferenceTsc,
                    HyperVFeatureFlag::ExtendedIoapicRte,
                ]
                .into(),
            },
        ]
    }

    #[test]
    fn reference_tsc_v1() {
        let v1_hv = GuestHypervisorInterface::HyperV {
            features: [HyperVFeatureFlag::ReferenceTsc].into(),
        };
        let spec = spec_with_interface(v1_hv.clone());
        let v1_spec = v1::instance_spec::InstanceSpec::try_from(spec).unwrap();
        assert_eq!(
            v8::components::board::GuestHypervisorInterface::from(
                v1_spec.board.guest_hv_interface
            ),
            v1_hv
        );
    }

    #[test]
    fn v8_features_require_v8() {
        for iface in v8_only_interfaces() {
            let spec = spec_with_interface(iface.clone());
            assert!(v6::instance_spec::InstanceSpec::try_from(spec.clone())
                .is_err());
            assert!(v1::instance_spec::InstanceSpec::try_from(spec.clone())
                .is_err());

            let v8_spec = v8::instance_spec::InstanceSpec::from(spec);
            assert_eq!(v8_spec.board.guest_hv_interface, iface);
        }
    }
}
