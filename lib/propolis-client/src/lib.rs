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

    impl From<handmade::api::DiskRequest> for generated::types::DiskRequest {
        fn from(req: handmade::api::DiskRequest) -> Self {
            let handmade::api::DiskRequest {
                name,
                slot,
                read_only,
                device,
                volume_construction_request,
            } = req;
            Self {
                name,
                slot: slot.into(),
                read_only,
                device,
                volume_construction_request: volume_construction_request.into(),
            }
        }
    }

    impl From<handmade::api::Slot> for generated::types::Slot {
        fn from(slot: handmade::api::Slot) -> Self {
            Self(slot.0)
        }
    }
}
