// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Version `EXT_MSI_DEST_ID` of the Propolis Server API.
//!
//! This version allows the bhyve guest hypervisor interface to specify a set
//! of optional features (initially, 15-bit Extended MSI Destination IDs), and
//! adds the `extended_ioapic_rte` Hyper-V feature flag.

pub mod api;
pub mod components;
pub mod instance_spec;
