// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Definitions for types exposed by the propolis-server API.
//!
//! This crate re-exports the latest versions of all API types from
//! `propolis-api-types-versions`. For versioned type access, depend
//! on that crate directly.

pub mod disk;
pub mod instance;
pub mod instance_spec;
pub mod migration;
pub mod serial;

// Re-export volume construction requests since they're part of a disk request.
pub use crucible_client_types::VolumeConstructionRequest;
