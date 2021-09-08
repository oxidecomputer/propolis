pub mod chipset;
pub mod ibmpc;
pub mod nvme;
pub mod pci;
pub mod ps2ctrl;
pub mod qemu;
pub mod rtc;
pub mod uart;
pub mod virtio;

#[cfg(feature = "crucible")]
pub mod crucible;
