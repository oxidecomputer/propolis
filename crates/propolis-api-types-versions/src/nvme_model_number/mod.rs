// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Version `NVME_MODEL_NUMBER` of the Propolis Server API.
//!
//! This version adds a `model_number` field to `NvmeDisk` and updates
//! `serial_number` to use the `serde_human_bytes` hex serialization.

pub mod api;
pub mod components;
pub mod instance_spec;
