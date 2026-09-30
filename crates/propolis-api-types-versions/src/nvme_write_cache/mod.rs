// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Version `NVME_WRITE_CACHE` of the Propolis Server API.
//!
//! This version adds an API field to control if an NVMe device reports having a
//! volatile write cache.

pub mod api;
pub mod components;
pub mod instance_spec;
