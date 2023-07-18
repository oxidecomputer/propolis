// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The Pheidippides framework: interfaces for creating and interacting with
//! VMs.

pub mod artifacts;
pub mod disk;
pub mod guest_os;
pub mod host_api;
pub mod port_allocator;
mod serial;
pub mod server_log_mode;
pub mod test_vm;

pub use test_vm::TestVm;
