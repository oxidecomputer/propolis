// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::collections::BTreeMap;

use propolis_api_types::instance::ReplacementComponent;
use propolis_api_types_versions::v1;
use serde::{Deserialize, Serialize};

use crate::spec::{api_spec_v0::ApiSpecError, Spec};

use super::MigrateError;

#[derive(Deserialize, Serialize, Debug)]
pub(crate) struct Preamble {
    pub instance_spec: v1::instance_spec::VersionedInstanceSpec,
    pub blobs: Vec<Vec<u8>>,
}

impl Preamble {
    pub fn new(
        instance_spec: v1::instance_spec::VersionedInstanceSpec,
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
        fn wrong_type_error(
            id: &v1::instance_spec::SpecKey,
            kind: &str,
        ) -> MigrateError {
            let msg =
                format!("component {id} is not a {kind} in the source spec");
            MigrateError::InstanceSpecsIncompatible(msg)
        }

        let v1::instance_spec::VersionedInstanceSpec::V0(mut source_spec) =
            self.instance_spec;
        for (id, comp) in replacements {
            let Some(to_amend) = source_spec.components.get_mut(id) else {
                return Err(MigrateError::InstanceSpecsIncompatible(format!(
                    "replacement component {id} not in source spec",
                )));
            };

            match comp {
                #[cfg(not(feature = "failure-injection"))]
                ReplacementComponent::MigrationFailureInjector(_) => {
                    return Err(MigrateError::InstanceSpecsIncompatible(
                        format!(
                            "replacing migration failure injector {id} is \
                            impossible because the feature is compiled out"
                        ),
                    ));
                }

                #[cfg(feature = "failure-injection")]
                ReplacementComponent::MigrationFailureInjector(comp) => {
                    let v1::instance_spec::Component::MigrationFailureInjector(
                        src,
                    ) = to_amend
                    else {
                        return Err(wrong_type_error(
                            id,
                            "migration failure injector",
                        ));
                    };

                    *src = comp.clone();
                }
                ReplacementComponent::CrucibleStorageBackend(comp) => {
                    let v1::instance_spec::Component::CrucibleStorageBackend(
                        src,
                    ) = to_amend
                    else {
                        return Err(wrong_type_error(id, "crucible backend"));
                    };

                    *src = comp.clone();
                }
                ReplacementComponent::VirtioNetworkBackend(comp) => {
                    let v1::instance_spec::Component::VirtioNetworkBackend(src) =
                        to_amend
                    else {
                        return Err(wrong_type_error(id, "viona backend"));
                    };

                    *src = comp.clone();
                }
            }
        }

        let amended_spec =
            source_spec.try_into().map_err(|e: ApiSpecError| {
                MigrateError::PreambleParse(e.to_string())
            })?;

        // TODO: Compare opaque blobs.

        Ok(amended_spec)
    }
}
