// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! ## Types to describe a VM which is being migrated.
//!
//! The API types that describe a VM are converted by `propolis-server` into a
//! [`struct Spec`][crate::lib::spec::Spec] as a "how this version happens to
//! describe VMs" internal structure. Early in migration we must convert this to
//! some format that a `propolis-server` of a different version can instantiate
//! an equivalent VM from, for device state everything else to be imported into.
//! We *kind of* use API types here, and the rest of this section gets into why
//! and what one should consider in adding future versions.
//!
//! Even for VMs that have been migrated many times, `propolis-server` must
//! incarnate a VM that can be described by *some* HTTP API `InstanceSpec`
//! version at some point in the past. We'll call this "oldest possible VM spec"
//! the "import horizon" that `propolis-server` supports.  Further, the tooling
//! for Dropshot (/OpenAPI) version management is quite good, and provides
//! guardrails against old versions' API types having structural changes.
//!
//! So, the strategy we use for communicating `struct Spec` is to convert it
//! back into *some* `InstanceSpec` that faithfully describes the Spec from
//! whence it came, send that on the wire to a destination `propolis-server`,
//! and expect it'll produce an equivalent VM to import state into.
//!
//! What complicates this is that it may not in general be possible to translate
//! a `struct Spec` into one of the most recent HTTP API `InstanceSpec` types.
//! We may have decided to split a setting into multiple fields, add a device
//! setting, or may have even added a field which cannot be simply "updated
//! into" from an old `InstanceSpec` in isolation (see: SMBIOS Type 1 tables!)
//! This points us away from the naive "`propolis-server` simply produces an
//! InstanceSpec from this version or last version" strategy one may imagine,
//! and towards keeping HTTP API types around well after those HTTP API
//! *endpoints* have been retired!
//!
//! So, we're finally at why the existing mechanics of getting a `Spec` out of
//! `propolis-server` and transmitted are what they are: go through a list of
//! `TryInto<Spec> for v*::instance_spec::InstanceSpec`, one of them will
//! succeed, and send that over. This is the implementation you'll find in
//! [`RonV0Runner::sync`][crate::lib::migrate::source::RonV0Runner::sync].
//!
//! ### [`VersionedInstanceSpec`]
//!
//! `fn sync` mentions `VersionedInstanceSpec`, which is where things get weirder
//! than simple HTTP API types.
//!
//! `propolis-server` supports more flexibility than the control plane is
//! expected to need in the immediate future. Given the current control plane
//! (update, live migration) plans, the source and destination `propolis-server`
//! may either be the same version, or the destination may be one release newer.
//!
//! Since we have to support HTTP API types as far back as `propolis-server`'s
//! import horizon, it's not much additional work to at least try supporting
//! migration across downgrades of `propolis-server`. If try converting to all
//! `v1, v2, v3 ..` forms of `InstanceSpec` in *ascending* order, the only time
//! conversion will fail to be downgradeable is if a VM has been created using
//! only-in-newest API language. This means that some VMs created using a
//! `latest::instance_spec::InstanceSpec` could end up with even `v1` types on
//! the wire for migration, but as long as `From/TryFrom` use is correct and
//! *not lossy*, that's fine!
//!
//! So, `VersionedInstanceSpec` is a container that is outside the HTTP API but
//! only contains OpenAPI-described API types. A destination `propolis-server`
//! is expected to gracefully reject new variants, and a source
//! `propolis-server` is expected to emit oldest-supported forms of instances.
//!
//! ### What if we didn't do all that?
//!
//! Another option to negotiate one `propolis-server`'s `struct Spec` into
//! another process would be to have some set of structs and functions to move
//! to and from totally-unrelated-to-HTTP-API wire format with more
//! forward-compatible device types. This would probably work! It would also
//! require testing work to check we don't inadvertently change the current
//! canonical definition of an "old version" which should never change.
//!
//! In either case we need testing that old device descriptions don't
//! *semantically* change, so it doesn't save effort there either.

use serde::{Deserialize, Serialize};

use propolis_api_types_versions::v1::instance::ReplacementComponent;
use propolis_api_types_versions::{v1, v2, v3, v6};

use std::collections::BTreeMap;

use crate::migrate::MigrateError;
use crate::spec::{
    api_spec_v1::ApiSpecError as V1SpecError,
    api_spec_v3::ApiSpecError as V3SpecError,
    api_spec_v6::ApiSpecError as V6SpecError, Spec,
};

/// A wrapper for one of any supported `InstanceSpec` that describe a
/// to-be-migrated VM.
///
/// Architecturally, this bridges the very fixed HTTP API types and the
/// possibility of having to migrate an arbitrarily old VM. See the doc comments
/// on [`migrate`][crate::lib::migrate] for more about how this all fits
/// together.
//
// If you're adding (or removing!?) API versions, you'll just want to adjust the
// variants here, plus uses in `VersionedInstanceSpec::from_spec` and
// `VersionedInstanceSpec::into_amended_spec`. shrimple as that.
#[derive(Deserialize, Serialize, Debug)]
pub(crate) enum VersionedInstanceSpec {
    V1(v1::instance_spec::InstanceSpec),
    V2(v2::instance_spec::InstanceSpec),
    V3(v3::instance_spec::InstanceSpec),
    V6(v6::instance_spec::InstanceSpec),
}

impl VersionedInstanceSpec {
    pub(crate) fn from_spec(
        spec: &Spec,
    ) -> Result<VersionedInstanceSpec, MigrateError> {
        // Try conversions in oldest-to-newest order in support of
        // migration-to-older-version. As long as the VM doesn't use a new
        // feature or setting, we'll pick a version the older Propolis should
        // know about, and everything else will "just work".
        //
        // When adding a new API version, the previous latest version will
        // probably have gone from having an `Into<Spec>` to instead having
        // `TryInto<Spec>`, which fails for a `Spec` describing whatever new
        // features have been added. The new latest version, hopefully, will
        // have an `Into<Spec>`. Those two versions should be the only ones that
        // need attention.
        let versioned = if let Ok(v1_spec) =
            TryInto::<v1::instance_spec::InstanceSpec>::try_into(spec.clone())
        {
            VersionedInstanceSpec::V1(v1_spec)
        } else if let Ok(v3_spec) =
            TryInto::<v3::instance_spec::InstanceSpec>::try_into(spec.clone())
        {
            VersionedInstanceSpec::V3(v3_spec)
        } else {
            VersionedInstanceSpec::V6(
                Into::<v6::instance_spec::InstanceSpec>::into(spec.clone()),
            )
        };
        Ok(versioned)
    }

    pub(crate) fn into_amended_spec(
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

        let amended_spec = match self {
            VersionedInstanceSpec::V1(mut source_spec) => {
                for (id, comp) in replacements {
                    let Some(to_amend) = source_spec.components.get_mut(id)
                    else {
                        return Err(MigrateError::InstanceSpecsIncompatible(
                            format!(
                                "replacement component {id} not in source spec",
                            ),
                        ));
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
                        ReplacementComponent::MigrationFailureInjector(
                            comp,
                        ) => {
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
            VersionedInstanceSpec::V2(_source_spec) => {
                panic!("should v2 really be here?");
            }
            VersionedInstanceSpec::V3(mut source_spec) => {
                for (id, comp) in replacements {
                    let Some(to_amend) = source_spec.components.get_mut(id)
                    else {
                        return Err(MigrateError::InstanceSpecsIncompatible(
                            format!(
                                "replacement component {id} not in source spec",
                            ),
                        ));
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
                        ReplacementComponent::MigrationFailureInjector(
                            comp,
                        ) => {
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

                let v6_spec: v6::instance_spec::InstanceSpec =
                    source_spec.into();
                let amended_spec: Spec =
                    v6_spec.try_into().map_err(|e: V6SpecError| {
                        let v3_error: V3SpecError = e.into();
                        MigrateError::PreambleParse(v3_error.to_string())
                    })?;

                amended_spec
            }
            VersionedInstanceSpec::V6(mut source_spec) => {
                for (id, comp) in replacements {
                    let Some(to_amend) = source_spec.components.get_mut(id)
                    else {
                        return Err(MigrateError::InstanceSpecsIncompatible(
                            format!(
                                "replacement component {id} not in source spec",
                            ),
                        ));
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
                        ReplacementComponent::MigrationFailureInjector(
                            comp,
                        ) => {
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

        Ok(amended_spec)
    }
}
