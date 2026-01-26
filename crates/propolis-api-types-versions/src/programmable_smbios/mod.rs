// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Version `PROGRAMMABLE_SMBIOS` of the Propolis Server API.
//!
//! This version adds support for programmable SMBIOS Type 1 tables,
//! allowing custom manufacturer, product name, serial number, and version
//! fields to be set on VMs.

pub mod api;
pub mod instance_spec;
