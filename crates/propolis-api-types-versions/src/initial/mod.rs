// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Version `INITIAL` of the Propolis Server API.
//!
//! This is the first version of the API, supporting basic VM operations
//! without programmable SMBIOS.

pub mod components;
pub mod disk;
pub mod instance;
pub mod instance_spec;
pub mod migration;
pub mod serial;
