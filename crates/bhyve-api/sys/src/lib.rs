// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

mod enums;
pub mod ioctls;
mod structs;
mod vmm_data;

pub use enums::*;
pub use ioctls::*;
pub use structs::*;
pub use vmm_data::*;

pub const VM_MAXCPU: usize = 32;

/// This is the VMM interface version which bhyve_api expects to operate
/// against.  All constants and structs defined by the crate are done so in
/// terms of that specific version.
pub const VMM_CURRENT_INTERFACE_VERSION: u32 = 15;
