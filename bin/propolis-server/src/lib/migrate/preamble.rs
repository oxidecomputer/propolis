// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::collections::BTreeMap;

use propolis_api_types::instance::ReplacementComponent;
use propolis_api_types_versions::{v1, v3, v6};
use serde::{Deserialize, Serialize};

use crate::migrate;
use crate::spec::{
    api_spec_v1::ApiSpecError as V1SpecError,
//    api_spec_v2::ApiSpecError as V2SpecError,
    api_spec_v3::ApiSpecError as V3SpecError,
    api_spec_v6::ApiSpecError as V6SpecError,
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
        fn wrong_type_error(
            id: &v1::instance_spec::SpecKey,
            kind: &str,
        ) -> MigrateError {
            let msg =
                format!("component {id} is not a {kind} in the source spec");
            MigrateError::InstanceSpecsIncompatible(msg)
        }

        let amended_spec = match self.instance_spec {
            migrate::types::VersionedInstanceSpec::V1(mut source_spec) => {
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

                let amended_spec: Spec =
                    source_spec.try_into().map_err(|e: V1SpecError| {
                        MigrateError::PreambleParse(e.to_string())
                    })?;

                amended_spec
            }
            migrate::types::VersionedInstanceSpec::V2(mut source_spec) => {
                panic!("source spec: {:?}", source_spec);
                /*
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

                let amended_spec: Spec =
                    source_spec.try_into().map_err(|e: V1SpecError| {
                        MigrateError::PreambleParse(e.to_string())
                    })?;

                amended_spec
                */
            }
            migrate::types::VersionedInstanceSpec::V3(mut source_spec) => {
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
                            let v3::instance_spec::Component::MigrationFailureInjector(
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
                            let v3::instance_spec::Component::CrucibleStorageBackend(
                                src,
                            ) = to_amend
                            else {
                                return Err(wrong_type_error(id, "crucible backend"));
                            };

                            *src = comp.clone();
                        }
                        ReplacementComponent::VirtioNetworkBackend(comp) => {
                            let v3::instance_spec::Component::VirtioNetworkBackend(src) =
                                to_amend
                            else {
                                return Err(wrong_type_error(id, "viona backend"));
                            };

                            *src = comp.clone();
                        }
                    }
                }

                let v6_spec: v6::instance_spec::InstanceSpec = source_spec.into();
                let amended_spec: Spec =
                    v6_spec.try_into().map_err(|e: V6SpecError| {
                        let v3_error: V3SpecError = e.into();
                        MigrateError::PreambleParse(v3_error.to_string())
                    })?;

                amended_spec
            }
            migrate::types::VersionedInstanceSpec::V6(mut source_spec) => {
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
                            let v6::instance_spec::Component::MigrationFailureInjector(
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
                            let v6::instance_spec::Component::CrucibleStorageBackend(
                                src,
                            ) = to_amend
                            else {
                                return Err(wrong_type_error(id, "crucible backend"));
                            };

                            *src = comp.clone();
                        }
                        ReplacementComponent::VirtioNetworkBackend(comp) => {
                            let v6::instance_spec::Component::VirtioNetworkBackend(src) =
                                to_amend
                            else {
                                return Err(wrong_type_error(id, "viona backend"));
                            };

                            *src = comp.clone();
                        }
                    }
                }

                let amended_spec: Spec =
                    source_spec.try_into().map_err(|e: V6SpecError| {
                        MigrateError::PreambleParse(e.to_string())
                    })?;

                amended_spec
            }
        };

        // TODO: Compare opaque blobs.

        Ok(amended_spec)
    }
}
