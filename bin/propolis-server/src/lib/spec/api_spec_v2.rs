// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Conversions between [`propolis_api_types_versions::v2`] instance specs and
//! the internal [`super::Spec`] representation.

use std::collections::BTreeMap;

use propolis_api_types::instance_spec::SpecKey;
use propolis_api_types_versions::{
    v1, v1::instance::ReplacementComponent, v2, v3,
};

use super::{builder::SpecBuilder, LegacyApiSpecError, Spec};
use crate::migrate::MigrateError;

#[cfg(feature = "failure-injection")]
use super::MigrationFailure;

impl TryFrom<Spec> for v2::instance_spec::InstanceSpec {
    type Error = LegacyApiSpecError;

    fn try_from(mut val: Spec) -> Result<Self, Self::Error> {
        // A V2 InstanceSpec is just a V1 InstanceSpec with an optional
        // `smbios_type1_input`.  Emptying out the SMBIOS Type 1 input means
        // this either can be converted to a V1 spec which we can losslessly
        // make V2 by adding the SMBIOS table input back in, or we wouldn't be
        // able to get to a V2 InstanceSpec either way.
        let smbios = val.smbios_type1_input.take();

        let v1::instance_spec::InstanceSpec { board, components } =
            val.try_into()?;

        Ok(v2::instance_spec::InstanceSpec { board, smbios, components })
    }
}

impl TryFrom<v2::instance_spec::InstanceSpec> for Spec {
    type Error = LegacyApiSpecError;

    fn try_from(
        value: v2::instance_spec::InstanceSpec,
    ) -> Result<Self, Self::Error> {
        Ok(v2_to_spec_builder(value)?.finish())
    }
}

/// Parses a v2 instance spec into a [`SpecBuilder`], validating component
/// names, PCI paths, and backend references along the way. Callers can add
/// additional (non-v2) components to the builder before calling `finish()`.
pub(crate) fn v2_to_spec_builder(
    value: v2::instance_spec::InstanceSpec,
) -> Result<SpecBuilder, LegacyApiSpecError> {
    let v3_spec: v3::instance_spec::InstanceSpec = value.into();

    crate::spec::api_spec_v3::v3_to_spec_builder(v3_spec)
}

pub(crate) fn amend(
    spec: &mut v2::instance_spec::InstanceSpec,
    replacements: &BTreeMap<SpecKey, ReplacementComponent>,
) -> Result<(), MigrateError> {
    for (id, replacement) in replacements {
        let Some(to_amend) = spec.components.get_mut(id) else {
            return Err(MigrateError::InstanceSpecsIncompatible(format!(
                "replacement component {id} not in source spec",
            )));
        };

        super::api_spec_v1::amend_component(id, to_amend, replacement)?;
    }

    Ok(())
}

#[cfg(test)]
mod test {
    use propolis_api_types_versions::latest::components::board::{
        Board, Chipset, Cpuid, GuestHypervisorInterface, I440Fx,
    };
    use propolis_api_types_versions::latest::{
        self,
        instance_spec::{CpuidVendor, SmbiosType1Input},
    };
    use propolis_api_types_versions::{v1, v2, v3, v6};

    #[test]
    fn smbios_type1() {
        let smbios_input = SmbiosType1Input {
            manufacturer: "a4x2".to_string(),
            product_name: "913-0000019".to_string(),
            serial_number: "2FAKE000".to_string(),
            version: 2,
        };

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
            },
            components: Default::default(),
            smbios: Some(smbios_input.clone()),
        };

        let spec =
            crate::spec::api_spec_latest::latest_to_spec_builder(api_spec)
                .unwrap()
                .finish();
        let smbios = spec
            .smbios_type1_input
            .as_ref()
            .expect("SMBIOS type 1 input preserved");
        assert_eq!(&smbios_input, smbios);

        let v6_spec =
            v6::instance_spec::InstanceSpec::try_from(spec.clone()).unwrap();
        let smbios =
            v6_spec.smbios.as_ref().expect("SMBIOS type 1 input preserved");
        assert_eq!(&smbios_input, smbios);

        let v3_spec =
            v3::instance_spec::InstanceSpec::try_from(spec.clone()).unwrap();
        let smbios =
            v3_spec.smbios.as_ref().expect("SMBIOS type 1 input preserved");
        assert_eq!(&smbios_input, smbios);

        let v2_spec =
            v2::instance_spec::InstanceSpec::try_from(spec.clone()).unwrap();
        let smbios =
            v2_spec.smbios.as_ref().expect("SMBIOS type 1 input preserved");
        assert_eq!(&smbios_input, smbios);

        let v1_res = v1::instance_spec::InstanceSpec::try_from(spec);
        assert!(v1_res.is_err());
    }
}
