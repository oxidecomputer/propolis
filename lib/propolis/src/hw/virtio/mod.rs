// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::sync::Arc;

#[allow(unused)]
mod bits;

pub mod block;
#[cfg(feature = "falcon")]
pub mod p9fs;
pub mod pci;
mod queue;
#[cfg(feature = "falcon")]
pub mod softnpu;
pub mod viona;

use crate::common::*;
use queue::VirtQueue;

pub use block::PciVirtioBlock;
pub use viona::PciVirtioViona;

pub trait VirtioDevice: Send + Sync + 'static + Lifecycle {
    /// Read/write device-specific virtio configuration space
    fn cfg_rw(&self, ro: RWOp);

    /// Get the device-specific virtio feature bits
    fn get_features(&self) -> u32;

    /// Set the device-specific virtio feature bits
    ///
    /// Returns `Err` if an error occurred while setting the features.  Doing so
    /// will transition the device to the Failed state.
    fn set_features(&self, feat: u32) -> Result<(), ()>;

    /// Service driver notification for a given virtqueue
    fn queue_notify(&self, vq: &Arc<VirtQueue>);

    /// Notification of virtqueue configuration change
    ///
    /// Returns `Err` if an error occurred while handling the specified
    /// `VqChange`.  Doing so will transition the device to the Failed state.
    fn queue_change(
        &self,
        _vq: &Arc<VirtQueue>,
        _change: VqChange,
    ) -> Result<(), ()> {
        Ok(())
    }
}

pub trait VirtioIntr: Send + 'static {
    fn notify(&self);
    fn read(&self) -> VqIntr;
}

pub enum VqChange {
    /// Underlying virtio device has been reset
    Reset,
    /// Physical address changed for VQ
    Address,
    /// MSI(-X) configuration changed for VQ
    IntrCfg,
}
pub enum VqIntr {
    /// Pin (lintr) interrupt
    Pin,
    /// MSI(-X) with address, data, and masked state
    Msi(u64, u32, bool),
}

#[usdt::provider(provider = "propolis")]
mod probes {
    fn virtio_vq_notify(virtio_dev_addr: u64, virtqueue_id: u16) {}
    fn virtio_vq_pop(vq_addr: u64, desc_idx: u16, avail_idx: u16) {}
    fn virtio_vq_push(vq_addr: u64, used_idx: u16, used_len: u32) {}
}
