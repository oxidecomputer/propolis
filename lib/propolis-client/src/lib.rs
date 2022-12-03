// Copyright 2022 Oxide Computer Company
//! A client for the Propolis hypervisor frontend's server API.
//!
//! It is being experimentally migrated to `progenitor` for auto-generation,
//! which is opt-in at present with crate feature `generated`, and additional
//! compatibility impls and re-exports to approximate the former handmade
//! bindings' module layout with crate feature `generated-migration`.
//!
//! Presently, when built with the `generated` flag, the legacy handmade
//! bindings are available in the `handmade` submodule.

#![cfg_attr(
    feature = "generated",
    doc = "This documentation was built with the `generated` feature **on**."
)]
#![cfg_attr(
    not(feature = "generated"),
    doc = "This documentation was built with the `generated` feature **off**."
)]

pub mod instance_spec;

#[cfg(feature = "generated")]
mod generated;
#[cfg(feature = "generated")]
pub use generated::*;

#[cfg(feature = "generated")]
pub mod handmade;
#[cfg(not(feature = "generated"))]
mod handmade;
#[cfg(not(feature = "generated"))]
pub use handmade::*;

#[cfg(feature = "generated-migration")]
pub use types as api;
#[cfg(feature = "generated-migration")]
mod _compat_impls {
    use super::{generated, handmade};
    use crucible_client_types::VolumeConstructionRequest as CrucibleVCR;
    use generated::types::VolumeConstructionRequest as GenVCR;

    impl Into<generated::types::DiskRequest> for handmade::api::DiskRequest {
        fn into(self) -> generated::types::DiskRequest {
            let Self {
                name,
                slot,
                read_only,
                device,
                volume_construction_request,
            } = self;
            generated::types::DiskRequest {
                name,
                slot: slot.into(),
                read_only,
                device,
                volume_construction_request: volume_construction_request.into(),
            }
        }
    }

    impl Into<generated::types::Slot> for handmade::api::Slot {
        fn into(self) -> generated::types::Slot {
            generated::types::Slot(self.0)
        }
    }

    impl From<CrucibleVCR> for GenVCR {
        fn from(vcr: CrucibleVCR) -> Self {
            match vcr {
                CrucibleVCR::Volume {
                    id,
                    block_size,
                    sub_volumes,
                    read_only_parent,
                } => GenVCR::Volume {
                    id,
                    block_size,
                    sub_volumes: sub_volumes
                        .into_iter()
                        .map(Into::into)
                        .collect(),
                    read_only_parent: read_only_parent
                        .map(|rop| Box::new((*rop).into())),
                },
                CrucibleVCR::Url { id, block_size, url } => {
                    GenVCR::Url { id, block_size, url }
                }
                CrucibleVCR::Region { block_size, opts, gen } => {
                    GenVCR::Region { block_size, opts: opts.into(), gen }
                }
                CrucibleVCR::File { id, block_size, path } => {
                    GenVCR::File { id, block_size, path }
                }
            }
        }
    }

    impl From<crucible_client_types::CrucibleOpts>
        for generated::types::CrucibleOpts
    {
        fn from(opts: crucible_client_types::CrucibleOpts) -> Self {
            let crucible_client_types::CrucibleOpts {
                id,
                target,
                lossy,
                flush_timeout,
                key,
                cert_pem,
                key_pem,
                root_cert_pem,
                control,
                read_only,
            } = opts;
            generated::types::CrucibleOpts {
                id,
                target: target.into_iter().map(|t| t.to_string()).collect(),
                lossy,
                flush_timeout,
                key,
                cert_pem,
                key_pem,
                root_cert_pem,
                control: control.map(|c| c.to_string()),
                read_only,
            }
        }
    }
}
