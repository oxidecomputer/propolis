use std::sync::Arc;

#[allow(unused)]
mod bits;

pub mod block;
mod pci;
mod queue;
pub mod viona;

use crate::common::*;
use crate::dispatch::DispCtx;
use queue::VirtQueue;

pub use block::VirtioBlock;

pub trait VirtioDevice: Send + Sync + 'static {
    fn device_cfg_rw(&self, ro: &mut RWOp);
    fn device_get_features(&self) -> u32;
    fn device_set_features(&self, feat: u32);
    fn queue_notify(&self, vq: &Arc<VirtQueue>, ctx: &DispCtx);

    #[allow(unused_variables)]
    fn device_reset(&self, ctx: &DispCtx) {}
    #[allow(unused_variables)]
    fn attach(&self, queues: &[Arc<VirtQueue>]) {}
    #[allow(unused_variables)]
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
    MSI(u64, u32, bool),
}
