// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::collections::BTreeMap;

use propolis_api_types::instance::ReplacementComponent;
use propolis_api_types_versions::v1;
use serde::{Deserialize, Serialize};

use crate::migrate;
use crate::spec::{
    Spec
};

use super::MigrateError;

#[derive(Deserialize, Serialize, Debug)]
pub(crate) struct Preamble {
    pub instance_spec: migrate::types::VersionedInstanceSpec,
    pub blobs: Vec<Vec<u8>>,
}

impl Preamble {
    pub fn new(
        instance_spec: migrate::types::VersionedInstanceSpec,
    ) -> Preamble {
        Preamble { instance_spec, blobs: Vec::new() }
    }

    /// Consume the spec in this Preamble and produce an instance spec suitable
    /// for initializing the target VM.
    ///
    /// This routine enumerates the disks and NICs in the `target_spec` and
    /// looks for disks with a Crucible backend and NICs with a viona backend.
    /// Any such backends will replace the corresponding backend entries in the
    /// source spec. If the target spec contains a replacement backend that is
    /// not present in the source spec, this routine fails.
    pub fn amend_spec(
        self,
        replacements: &BTreeMap<
            v1::instance_spec::SpecKey,
            ReplacementComponent,
        >,
    ) -> Result<Spec, MigrateError> {
        let amended_spec = self.instance_spec.into_amended_spec(replacements)?;

        // TODO: Compare opaque blobs.

        Ok(amended_spec)
    }
}
