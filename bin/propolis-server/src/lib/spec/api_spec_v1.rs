// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Conversions from version-1 instance specs in the [`propolis_api_types`]
//! crate to the internal [`super::Spec`] representation.

use crate::spec::api_spec_v0::ApiSpecError;
use propolis_api_types::instance_spec::{
    v0::InstanceSpecV0, v1::InstanceSpecV1,
};
use thiserror::Error;

use super::Spec;

#[derive(Debug, Error)]
#[error("missing smbios_type1_input")]
pub(crate) struct MissingSmbiosError;

impl TryFrom<Spec> for InstanceSpecV1 {
    type Error = MissingSmbiosError;

    fn try_from(val: Spec) -> Result<Self, Self::Error> {
        let Some(smbios) = val.smbios_type1_input.clone() else {
            return Err(MissingSmbiosError);
        };

        let InstanceSpecV0 { board, components } = InstanceSpecV0::from(val);
        Ok(InstanceSpecV1 { board, components, smbios })
    }
}

impl TryFrom<InstanceSpecV1> for Spec {
    type Error = ApiSpecError;

    fn try_from(value: InstanceSpecV1) -> Result<Self, Self::Error> {
        let InstanceSpecV1 { board, components, smbios } = value;
        let v0 = InstanceSpecV0 { board, components };
        let mut spec: Spec = v0.try_into()?;
        spec.smbios_type1_input = Some(smbios);
        Ok(spec)
    }
}
