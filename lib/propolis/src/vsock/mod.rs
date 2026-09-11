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
#[error("guest cid {0} contains reserved bits")]
pub struct InvalidGuestCid(u64);

#[derive(Debug, Copy, Clone)]
pub struct GuestCid(u64);

impl GuestCid {
    pub const fn get(&self) -> u64 {
        self.0
    }
}

impl TryFrom<u64> for GuestCid {
    type Error = InvalidGuestCid;

    fn try_from(value: u64) -> Result<Self, Self::Error> {
        match value {
            // Within the virtio spec cid 0,1, and 2 have special meaning.
            cid @ 0..=2 => Err(InvalidGuestCid(cid)),
            // The upper 32 bits of the cid are reserved
            cid if cid >> 32 != 0 => Err(InvalidGuestCid(value)),
            // This cid is valid
            cid => Ok(GuestCid(cid)),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum VsockError {
    #[error("failed to send virt queue notification for queue {}", queue)]
    QueueNotify { queue: u16 },
}

pub trait VsockBackend: Send + Sync + 'static {
    fn queue_notify(&self, queue_id: u16) -> Result<(), VsockError>;
}

#[usdt::provider(provider = "propolis")]
mod probes {
    use crate::vsock::packet::VsockPacketHeader;

    /// Host->Guest
    fn vsock_pkt_rx(hdr: &VsockPacketHeader) {}
    /// Guest->Host
    fn vsock_pkt_tx(hdr: &VsockPacketHeader) {}
}
