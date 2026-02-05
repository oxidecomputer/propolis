use std::thread::JoinHandle;

use iddqd::IdHashMap;
use slog::Logger;

use crate::hw::virtio::vsock::VsockVq;
use crate::vsock::proxy::VsockPortMapping;

bitflags! {
    pub struct PollEvents: i32 {
        const IN = libc::POLLIN as i32;
        const OUT = libc::POLLOUT as i32;
    }
}

pub struct VsockPollerNotify;

impl VsockPollerNotify {
    pub fn queue_notify(&self, _id: u16) -> std::io::Result<()> {
        return Err(std::io::Error::other(
            "not available on non-illumos systems",
        ));
    }
}

pub struct VsockPoller;

impl VsockPoller {
    pub fn new(
        _cid: u32,
        _queues: VsockVq,
        _log: Logger,
        _port_mappings: IdHashMap<VsockPortMapping>,
    ) -> std::io::Result<Self> {
        return Err(std::io::Error::other(
            "VsockPoller is not available on non-illumos systems",
        ));
    }

    pub fn notify_handle(&self) -> VsockPollerNotify {
        VsockPollerNotify {}
    }

    pub fn run(self) -> JoinHandle<()> {
        std::thread::Builder::new()
            .name("vsock-event-loop".to_string())
            .spawn(move || {})
            .expect("failed to spawn vsock event loop")
    }
}
