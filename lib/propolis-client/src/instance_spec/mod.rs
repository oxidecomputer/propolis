// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Instance specifications: abstract descriptions of a VM's devices and config.
//!
//! An instance spec describes a VM's virtual devices, backends, and other
//! guest environment configuration supplied by the Propolis VMM. RFD 283
//! contains more details about how specs are used throughout the Oxide stack.
//!
//! # Module layout
//!
//! TODO(gjc)
//!
//! # Versioning & compatibility
//!
//! TODO(gjc)

#![allow(rustdoc::private_intra_doc_links)]

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub use propolis_types::PciPath;

pub mod components;
pub mod migration;
pub mod v0;

/// Type alias for keys in the instance spec's maps.
type SpecKey = String;

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields, tag = "version", content = "spec")]
pub enum VersionedInstanceSpec {
    V0(v0::InstanceSpecV0),
}
