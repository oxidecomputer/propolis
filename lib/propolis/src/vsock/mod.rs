pub mod buffer;
pub mod packet;
pub mod poller;
pub mod proxy;

pub use proxy::VsockProxy;

/// Well-known CID for the host
pub(crate) const VSOCK_HOST_CID: u64 = 2;

#[derive(Debug, thiserror::Error)]
pub enum VsockError {
    #[error("failed to send virt queue notification for queue {0}")]
    QueueNotify(u16),
}

pub trait VsockBackend: Send + Sync + 'static {
    fn queue_notify(&self, queue_id: u16) -> Result<(), VsockError>;
}
