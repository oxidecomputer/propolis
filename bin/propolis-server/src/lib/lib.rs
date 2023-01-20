// always present
pub mod config;
pub mod mock_server;
mod serial;
#[cfg_attr(feature = "mock-only", allow(unused))]
mod spec;

#[cfg(not(feature = "mock-only"))]
mod initializer;
#[cfg(not(feature = "mock-only"))]
mod migrate;
#[cfg(not(feature = "mock-only"))]
pub mod server;
#[cfg(not(feature = "mock-only"))]
mod stats;
#[cfg(not(feature = "mock-only"))]
mod vcpu_tasks;
#[cfg(not(feature = "mock-only"))]
mod vm;
#[cfg(not(feature = "mock-only"))]
pub mod vnc;
