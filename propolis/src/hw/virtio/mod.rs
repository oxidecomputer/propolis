use std::sync::Arc;

#[allow(unused)]
mod bits;

pub mod block;
pub mod pci;
mod queue;
pub mod viona;

use crate::common::*;
use crate::dispatch::DispCtx;
use queue::VirtQueue;

pub use block::PciVirtioBlock;
pub use viona::PciVirtioViona;

pub trait VirtioDevice: Send + Sync + 'static + Entity {
    /// Read/write device-specific virtio configuration space
    fn cfg_rw(&self, ro: RWOp);
    /// Get the device-specific virtio feature bits
    fn get_features(&self) -> u32;
    /// Set the device-specific virtio feature bits
    fn set_features(&self, feat: u32);
    /// Service driver notification for a given virtqueue
    fn queue_notify(&self, vq: &Arc<VirtQueue>, ctx: &DispCtx);

    #[allow(unused_variables)]
    /// Device-wide reset actions during virtio reset
    fn reset(&self, ctx: &DispCtx) {}

    #[allow(unused_variables)]
    /// Notification of virtqueue configuration change
    fn queue_change(
        &self,
        vq: &Arc<VirtQueue>,
        change: VqChange,
        ctx: &DispCtx,
    ) {
    }
}

pub trait VirtioIntr: Send + 'static {
    fn notify(&self, ctx: &DispCtx);
    fn read(&self) -> VqIntr;
}

pub enum VqChange {
    Reset,
    Address,
    IntrCfg,
}
pub enum VqIntr {
    // Pin (lintr) interrupt
    Pin,
    /// MSI(-X) with address, data, and masked state
    Msi(u64, u32, bool),
}
