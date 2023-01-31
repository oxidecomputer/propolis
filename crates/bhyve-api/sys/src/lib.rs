mod enums;
pub mod ioctls;
mod structs;
mod vmm_data;

pub use enums::*;
pub use ioctls::*;
pub use structs::*;
pub use vmm_data::*;

pub const VM_MAXCPU: usize = 32;

/// This is the VMM interface version against which bhyve_api expects to operate
/// against.  All constants and structs defined by the crate are done so in
/// terms of that specific version.
pub const VMM_CURRENT_INTERFACE_VERSION: u32 = 8;
