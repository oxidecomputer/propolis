cfg_if::cfg_if! {
    if #[cfg(target_os = "illumos")] {
        mod kvm;
        pub use kvm::*;
    } else {
        mod stubs;
        pub use stubs::*;
    }
}
