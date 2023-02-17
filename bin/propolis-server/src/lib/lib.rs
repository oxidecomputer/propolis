// always present
pub mod config;
#[cfg(any(test, feature = "mock-only"))]
pub mod mock_server;
mod serial;
#[cfg_attr(feature = "mock-only", allow(unused))]
mod spec;

cfg_if::cfg_if! {
    if #[cfg(not(feature = "mock-only"))] {
        mod initializer;
        mod migrate;
        pub mod server;
        mod stats;
        mod vcpu_tasks;
        mod vm;
        pub mod vnc;
    }
}
