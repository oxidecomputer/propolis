// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::collections::HashMap;

use propolis_api_types::{
    instance_spec::{SpecKey, VersionedInstanceSpec},
    ReplacementComponent,
};
use serde::{Deserialize, Serialize};

use crate::spec::{api_spec_v0::ApiSpecError, Spec};

use super::MigrateError;

#[derive(Deserialize, Serialize, Debug)]
pub(crate) struct Preamble {
    pub instance_spec: VersionedInstanceSpec,
    pub blobs: Vec<Vec<u8>>,
}

impl Preamble {
    pub fn new(instance_spec: VersionedInstanceSpec) -> Preamble {
        Preamble { instance_spec, blobs: Vec::new() }
    }

    /// Creates a migration target instance spec by taking the spec supplied in
    /// this preamble and replacing any backend components found therein with
    /// the backend components listed in `replace_components`.
    pub fn get_amended_spec(
        self,
        replace_components: &HashMap<SpecKey, ReplacementComponent>,
    ) -> Result<Spec, MigrateError> {
        let VersionedInstanceSpec::V0(mut v0_spec) = self.instance_spec;
        for (id, component) in replace_components {
            match v0_spec.components.get_mut(id) {
                Some(ent) => {
                    *ent = component.clone().into();
                }
                None => {
                    return Err(MigrateError::InstanceSpecsIncompatible(
                        format!(
                            "replacement component {id} not in source spec",
                        ),
                    ));
                }
            }
        }

        Spec::try_from(v0_spec).map_err(|e: ApiSpecError| {
            MigrateError::PreambleParse(e.to_string())
        })
    }
}
