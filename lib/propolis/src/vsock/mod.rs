// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

pub mod buffer;
pub mod packet;

#[cfg(target_os = "illumos")]
pub mod poller;

#[cfg(not(target_os = "illumos"))]
#[path = "poller_stub.rs"]
pub mod poller;

pub mod proxy;
pub use proxy::VsockProxy;

/// Well-known CID for the host
pub(crate) const VSOCK_HOST_CID: u64 = 2;

#[derive(Debug, thiserror::Error)]
pub enum VsockError {
    #[error("failed to send virt queue notification for queue {}", queue)]
    QueueNotify { queue: u16 },
}

pub trait VsockBackend: Send + Sync + 'static {
    fn queue_notify(&self, queue_id: u16) -> Result<(), VsockError>;
}
