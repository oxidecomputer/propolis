#[cfg(target_os = "illumos")]
mod kvm;

#[cfg(target_os = "illumos")]
pub use kvm::*;

#[cfg(not(target_os = "illumos"))]
mod stubs;

#[cfg(not(target_os = "illumos"))]
pub use stubs::*;
