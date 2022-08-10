//! The Pheidippides framework: interfaces for creating and interacting with
//! VMs.

pub mod artifacts;
pub mod guest_os;
pub mod host_api;
mod serial;
pub mod test_vm;

pub use test_vm::TestVm;
