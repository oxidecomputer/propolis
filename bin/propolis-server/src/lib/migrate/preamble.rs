// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use propolis_api_types::instance_spec::VersionedInstanceSpec;
use serde::{Deserialize, Serialize};

use crate::spec::{self, api_spec_v0::ApiSpecParseError};

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

    /// Checks to see if the serialized spec in this preamble is compatible with
    /// the supplied `other_spec`.
    ///
    /// This check runs on the destination.
    pub fn check_compatibility(
        self,
        other_spec: &spec::Spec,
    ) -> Result<(), MigrateError> {
        let VersionedInstanceSpec::V0(v0_spec) = self.instance_spec;
        let this_spec: spec::Spec =
            v0_spec.try_into().map_err(|e: ApiSpecParseError| {
                MigrateError::PreambleParse(e.to_string())
            })?;

        this_spec.is_migration_compatible(other_spec).map_err(|e| {
            MigrateError::InstanceSpecsIncompatible(e.to_string())
        })?;

        // TODO: Compare opaque blobs.

        Ok(())
    }
}
