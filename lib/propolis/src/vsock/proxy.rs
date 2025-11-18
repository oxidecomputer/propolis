use std::net::TcpStream;
use std::num::NonZeroUsize;
use std::num::Wrapping;
use std::os::fd::AsRawFd;
use std::os::fd::RawFd;
use std::thread::JoinHandle;

use crate::hw::virtio::vsock::VsockVq;
use crate::vsock::buffer::VsockBuf;
use crate::vsock::packet::VsockPacket;
use crate::vsock::poller::VsockPoller;
use crate::vsock::poller::VsockPollerNotify;
use crate::vsock::VsockBackend;

/// Default buffer size for guest->host data.
pub(crate) const CONN_TX_BUF_SIZE: usize = 1024 * 128;

/// Connection lifecycle state for a vsock connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConnState {
    /// Connection is fully established and can send/receive data.
    Established,
    /// Host socket closed for reading (received EOF).
    /// We can still write data to the socket but won't receive more.
    ShutdownRead,
    /// Guest requested shutdown of send direction.
    /// We can still read from socket but won't send more to it.
    ShutdownWrite,
    /// Both directions are shut down, connection is closing.
    Closing,
}

#[derive(Debug, Clone, Copy, Eq, Hash, PartialEq)]
pub(crate) struct ConnKey {
    /// The port the guest is transmitting to.
    pub(crate) host_port: u32,
    /// The port the guest is transmitting from.
    pub(crate) guest_port: u32,
}

impl ConnKey {
    /// Pack into a usize to avoid boxing when passing to port_associate.
    pub(crate) fn to_usize(self) -> usize {
        ((self.host_port as usize) << 32) | (self.guest_port as usize)
    }

    /// Unpack from a usize.
    pub(crate) fn from_usize(val: usize) -> Self {
        Self { host_port: (val >> 32) as u32, guest_port: val as u32 }
    }
}

/// An established guest<=>host connection
#[derive(Debug)]
pub(crate) struct VsockProxyConn {
    pub(crate) socket: TcpStream,
    /// Current connection state.
    state: ConnState,
    /// Ring buffer used to receive packets from the guest tx virt queue.
    vbuf: VsockBuf,
    /// Bytes we've consumed from vbuf (forwarded to socket). Reported to guest.
    fwd_cnt: Wrapping<u32>,
    /// The fwd_cnt value we last sent to the guest in a credit update.
    last_fwd_cnt_sent: Wrapping<u32>,
    /// Bytes we've sent to the guest from the socket.
    tx_cnt: Wrapping<u32>,
    /// Guest's buffer allocation (how much buffer space they have).
    peer_buf_alloc: u32,
    /// Bytes the guest has consumed from their buffer.
    peer_fwd_cnt: Wrapping<u32>,
}

impl VsockProxyConn {
    pub fn new() -> Self {
        let socket = TcpStream::connect("127.0.0.1:3000").unwrap();
        socket.set_nonblocking(true).expect("failed to set socket nonblocking");

        Self {
            socket,
            state: ConnState::Established,
            vbuf: VsockBuf::new(NonZeroUsize::new(CONN_TX_BUF_SIZE).unwrap()),
            fwd_cnt: Wrapping(0),
            last_fwd_cnt_sent: Wrapping(0),
            tx_cnt: Wrapping(0),
            peer_buf_alloc: 0,
            peer_fwd_cnt: Wrapping(0),
        }
    }

    pub fn has_buffered_data(&self) -> bool {
        !self.vbuf.is_empty()
    }

    /// Check if the connection can read from the host socket.
    pub fn can_read(&self) -> bool {
        matches!(self.state, ConnState::Established | ConnState::ShutdownWrite)
    }

    /// Check if the connection can write to the host socket.
    pub fn can_write(&self) -> bool {
        matches!(self.state, ConnState::Established | ConnState::ShutdownRead)
    }

    /// Mark that we received EOF from the host socket.
    pub fn shutdown_read(&mut self) {
        self.state = match self.state {
            ConnState::Established => ConnState::ShutdownRead,
            ConnState::ShutdownWrite => ConnState::Closing,
            other => other,
        };
    }

    /// Mark that the guest requested shutdown of the send direction.
    pub fn shutdown_write(&mut self) {
        self.state = match self.state {
            ConnState::Established => ConnState::ShutdownWrite,
            ConnState::ShutdownRead => ConnState::Closing,
            other => other,
        };
    }

    /// Check if the connection should be removed.
    pub fn should_close(&self) -> bool {
        self.state == ConnState::Closing
    }

    /// Update peer credit info from a packet header.
    pub fn update_peer_credit(&mut self, buf_alloc: u32, fwd_cnt: u32) {
        self.peer_buf_alloc = buf_alloc;
        self.peer_fwd_cnt = Wrapping(fwd_cnt);
    }

    /// Process a packet received from the guest tx queue.
    pub fn recv_packet(&mut self, packet: VsockPacket) {
        self.vbuf.push(packet.data).unwrap();
        self.update_peer_credit(
            packet.header.buf_alloc(),
            packet.header.fwd_cnt(),
        );
    }

    pub fn flush(&mut self) -> std::io::Result<usize> {
        self.vbuf.write_to(&mut self.socket)
    }

    /// Calculate how much data we can send to the guest based on their credit.
    pub fn peer_credit(&self) -> u32 {
        let in_flight = (self.tx_cnt - self.peer_fwd_cnt).0;
        self.peer_buf_alloc.saturating_sub(in_flight)
    }

    /// Update fwd_cnt after consuming data from vbuf.
    pub fn update_fwd_cnt(&mut self, bytes: u32) {
        self.fwd_cnt += Wrapping(bytes);
    }

    /// Update tx_cnt after sending data to guest.
    pub fn update_tx_cnt(&mut self, bytes: u32) {
        self.tx_cnt += Wrapping(bytes);
    }

    /// Get our current fwd_cnt to report to the guest.
    pub fn fwd_cnt(&self) -> u32 {
        self.fwd_cnt.0
    }

    /// Get our buffer allocation to report to the guest.
    pub fn buf_alloc(&self) -> u32 {
        self.vbuf.capacity() as u32
    }

    /// Check if we should send a credit update to the guest.
    ///
    /// Returns true if we've consumed more than half of our buffer capacity
    /// since the last credit update was sent.
    pub fn needs_credit_update(&self) -> bool {
        let bytes_consumed_since_update =
            (self.fwd_cnt - self.last_fwd_cnt_sent).0;
        bytes_consumed_since_update > (self.vbuf.capacity() / 2) as u32
    }

    /// Mark that we've sent a credit update with the current fwd_cnt.
    pub fn mark_credit_sent(&mut self) {
        self.last_fwd_cnt_sent = self.fwd_cnt;
    }

    pub fn get_fd(&self) -> RawFd {
        self.socket.as_raw_fd()
    }
}

/// virtio-socket backend that proxies between a guest and a host UDS.
pub struct VsockProxy {
    poller: VsockPollerNotify,
    _evloop_handle: JoinHandle<()>,
}

impl VsockProxy {
    pub fn new(cid: u32, queues: VsockVq) -> Self {
        let evloop = VsockPoller::new(cid, queues).unwrap();
        let poller = evloop.notify_handle();
        let jh = evloop.run();

        Self { poller, _evloop_handle: jh }
    }

    /// Notification from the vsock device that one of the queues has had an
    /// event.
    fn queue_notify(&self, vq_id: u16) {
        self.poller.queue_notify(vq_id).unwrap();
    }
}

impl VsockBackend for VsockProxy {
    fn queue_notify(&self, queue_id: u16) {
        self.queue_notify(queue_id);
    }
}
