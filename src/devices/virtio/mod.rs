mod block;
mod pci;
mod queue;

use crate::common::*;
use crate::dispatch::DispCtx;
use queue::VirtQueue;

pub use block::VirtioBlock;

pub trait VirtioDevice: Send + Sync {
    fn device_cfg_size() -> usize;
    fn device_cfg_rw(&self, ro: &mut RWOp);
    fn device_get_features(&self) -> u32;
    fn device_set_features(&self, feat: u32);
    fn device_id_and_class() -> (u16, u8);
    fn queue_notify(&self, qid: u16, vq: &VirtQueue, ctx: &DispCtx);
}
