mod enums;
mod ioctls;
mod structs;

pub use enums::*;
pub use ioctls::*;
pub use structs::*;

pub const VM_MAXCPU: usize = 32;

pub const VMM_PATH_PREFIX: &str = "/dev/vmm";
pub const VMM_CTL_PATH: &str = "/dev/vmmctl";
