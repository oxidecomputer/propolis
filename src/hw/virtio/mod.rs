use std::sync::Arc;

#[allow(unused)]
mod bits;

pub mod block;
pub mod viona;
mod pci;
mod queue;

use crate::common::*;
use crate::dispatch::DispCtx;
use queue::VirtQueue;

pub use block::VirtioBlock;

pub trait VirtioDevice: Send + Sync + 'static {
    fn device_cfg_rw(&self, ro: &mut RWOp);
    fn device_get_features(&self) -> u32;
    fn device_set_features(&self, feat: u32);
    fn queue_notify(&self, qid: u16, vq: &Arc<VirtQueue>, ctx: &DispCtx);
}

trait VirtioIntr: Send + 'static {
    fn notify(&self, ctx: &DispCtx);
}
