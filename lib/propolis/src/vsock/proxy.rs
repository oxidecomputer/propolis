// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::net::SocketAddr;
use std::net::TcpStream;
use std::num::NonZeroUsize;
use std::num::Wrapping;
use std::os::fd::AsRawFd;
use std::os::fd::RawFd;
use std::thread::JoinHandle;
use std::time::Duration;

use iddqd::IdHashItem;
use iddqd::IdHashMap;
use serde::Deserialize;
use slog::error;
use slog::Logger;

use crate::hw::virtio::vsock::VsockVq;
use crate::vsock::buffer::VsockBuf;
use crate::vsock::buffer::VsockBufError;
use crate::vsock::packet::VsockPacket;
use crate::vsock::packet::VsockPacketHeader;
use crate::vsock::poller::PollEvents;
use crate::vsock::poller::VsockPoller;
use crate::vsock::poller::VsockPollerNotify;
use crate::vsock::VsockBackend;
use crate::vsock::VsockError;

/// Default buffer size for guest->host data.
pub const CONN_TX_BUF_SIZE: usize = 1024 * 128;

/// Connection lifecycle state for a vsock connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnState {
    // The guest has sent us a VIRTIO_VSOCK_OP_REQUEST
    Init,
    /// We have sent VIRTIO_VSOCK_OP_RESPONSE - connection can send/recv data
    Established,
    /// The connection is in the process of closing - read and write halves are
    /// tracked seperately
    Closing {
        read: bool,
        write: bool,
    },
}

#[derive(Debug, Clone, Copy, Eq, Hash, PartialEq)]
pub struct ConnKey {
    /// The port the guest is transmitting to.
    pub(crate) host_port: u32,
    /// The port the guest is transmitting from.
    pub(crate) guest_port: u32,
}

// This impl allows us to convert to and from a portev_user object (see
// port_associate3C). The conversion to and from a usize allows us to encode
// the key in the pointer value itself rather than allocating memory.
//
// NB: This object is defined as a `*mut c_void` and therefore will not be
// 64bits on all platforms, but we currently only support x86_64 hardware,
// therefore we are leaving a static assertion behind as a future hint to
// ourselves.
impl ConnKey {
    /// Pack the host + port into a usize
    pub fn to_portev_user(self) -> usize {
        static_assertions::assert_eq_size!(u64, usize);
        ((self.host_port as usize) << 32) | (self.guest_port as usize)
    }

    /// Unpack the host + port from a usize
    pub fn from_portev_user(val: usize) -> Self {
        Self { host_port: (val >> 32) as u32, guest_port: val as u32 }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ProxyConnError {
    #[error("Failed to connect to vsock backend {backend}: {source}")]
    Socket {
        backend: SocketAddr,
        #[source]
        source: std::io::Error,
    },
    #[error("Failed to put socket into nonblocking mode: {0}")]
    NonBlocking(#[source] std::io::Error),
    #[error("Cannot transition connection from {from:?} to {to:?}")]
    InvalidStateTransition { from: ConnState, to: ConnState },
}

/// An established guest<=>host connection.
///
/// Note that the internal state of the proxy connection uses `Wrapping<u32>`
/// because the virtio spec uses the following calculation to determine how much
/// buffer space a guest has:
///
/// /* tx_cnt is the sender's free-running bytes transmitted counter */
/// u32 peer_free = peer_buf_alloc - (tx_cnt - peer_fwd_cnt);
///
/// The lifetime of a connection can exceed u32::MAX bytes transmitted, so we
/// rely on wrapping semantics to determine the difference.
#[derive(Debug)]
pub struct VsockProxyConn {
    pub(crate) socket: TcpStream,
    /// Current connection state.
    state: ConnState,
    /// Ring buffer used to receive packets from the guest tx virt queue.
    vbuf: VsockBuf,
    /// Bytes we've consumed from vbuf (forwarded to socket).
    fwd_cnt: Wrapping<u32>,
    /// The fwd_cnt value we last sent to the guest in a credit update.
    last_fwd_cnt_sent: Wrapping<u32>,
    /// Bytes we've sent to the guest from the socket.
    tx_cnt: Wrapping<u32>,
    /// Guest's buffer allocation.
    peer_buf_alloc: u32,
    /// Bytes the guest has consumed from their buffer.
    peer_fwd_cnt: Wrapping<u32>,
}

impl VsockProxyConn {
    /// Create a new `VsockProxyConn` connected to an underlying host socket.
    pub fn new(addr: &SocketAddr) -> Result<Self, ProxyConnError> {
        let socket =
            TcpStream::connect_timeout(addr, Duration::from_millis(100))
                .map_err(|e| ProxyConnError::Socket {
                    backend: *addr,
                    source: e,
                })?;
        socket.set_nonblocking(true).map_err(ProxyConnError::NonBlocking)?;

        Ok(Self {
            socket,
            state: ConnState::Init,
            vbuf: VsockBuf::new(NonZeroUsize::new(CONN_TX_BUF_SIZE).unwrap()),
            fwd_cnt: Wrapping(0),
            last_fwd_cnt_sent: Wrapping(0),
            tx_cnt: Wrapping(0),
            peer_buf_alloc: 0,
            peer_fwd_cnt: Wrapping(0),
        })
    }

    /// Set of `PollEvents` that this connection is interested in.
    pub fn poll_interests(&self) -> Option<PollEvents> {
        let mut interests = PollEvents::empty();
        interests.set(PollEvents::OUT, self.has_buffered_data());
        interests.set(PollEvents::IN, self.guest_can_read());

        match interests.is_empty() {
            true => None,
            false => Some(interests),
        }
    }

    /// Returns `true` if the connection has data pending in its ring buffer
    /// that needs to be flushed to the underlying socket.
    pub fn has_buffered_data(&self) -> bool {
        !self.vbuf.is_empty()
    }

    /// Set the connection to established.
    pub fn set_established(&mut self) -> Result<(), ProxyConnError> {
        match self.state {
            ConnState::Init => self.state = ConnState::Established,
            current => {
                return Err(ProxyConnError::InvalidStateTransition {
                    from: current,
                    to: ConnState::Established,
                })
            }
        }

        Ok(())
    }

    /// Check if the connection can read from the host socket.
    pub fn guest_can_read(&self) -> bool {
        matches!(
            self.state,
            ConnState::Established | ConnState::Closing { read: false, .. }
        )
    }

    pub fn shutdown_guest_read(&mut self) -> Result<(), ProxyConnError> {
        self.state = match self.state {
            ConnState::Established => {
                ConnState::Closing { read: true, write: false }
            }
            ConnState::Closing { write, .. } => {
                ConnState::Closing { read: true, write: write }
            }
            current => {
                return Err(ProxyConnError::InvalidStateTransition {
                    from: current,
                    to: ConnState::Closing { read: true, write: false },
                })
            }
        };

        Ok(())
    }

    pub fn shutdown_guest_write(&mut self) -> Result<(), ProxyConnError> {
        self.state = match self.state {
            ConnState::Established => {
                ConnState::Closing { read: false, write: true }
            }
            ConnState::Closing { read, .. } => {
                ConnState::Closing { read, write: true }
            }
            current => {
                return Err(ProxyConnError::InvalidStateTransition {
                    from: current,
                    to: ConnState::Closing { read: true, write: false },
                })
            }
        };

        Ok(())
    }

    /// Check if the connection should be removed.
    pub fn should_close(&self) -> bool {
        matches!(self.state, ConnState::Closing { read: true, write: true })
    }

    /// Update peer credit info from a packet header.
    pub fn update_peer_credit(&mut self, header: &VsockPacketHeader) {
        self.peer_buf_alloc = header.buf_alloc();
        self.peer_fwd_cnt = Wrapping(header.fwd_cnt());
    }

    /// Process a packet received from the guest tx queue.
    pub fn recv_packet(
        &mut self,
        packet: VsockPacket,
    ) -> Result<(), VsockBufError> {
        self.vbuf.push(packet.data)
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

#[derive(Deserialize, Debug, Clone, Copy)]
pub struct VsockPortMapping {
    port: u32,
    // TODO this could be extended to support Unix sockets as well.
    addr: SocketAddr,
}

impl VsockPortMapping {
    pub fn new(port: u32, addr: SocketAddr) -> Self {
        Self { port, addr }
    }

    pub fn addr(&self) -> &SocketAddr {
        &self.addr
    }
}

impl IdHashItem for VsockPortMapping {
    type Key<'a> = u32;

    fn key(&self) -> Self::Key<'_> {
        self.port
    }

    iddqd::id_upcast!();
}

/// virtio-socket backend that proxies between a guest and a host UDS.
pub struct VsockProxy {
    log: Logger,
    poller: VsockPollerNotify,
    _evloop_handle: JoinHandle<()>,
}

impl VsockProxy {
    pub fn new(
        cid: u32,
        queues: VsockVq,
        log: Logger,
        port_mappings: IdHashMap<VsockPortMapping>,
    ) -> Self {
        let evloop =
            VsockPoller::new(cid, queues, log.clone(), port_mappings).unwrap();
        let poller = evloop.notify_handle();
        let jh = evloop.run();

        Self { log, poller, _evloop_handle: jh }
    }

    /// Notification from the vsock device that one of the queues has had an
    /// event.
    fn queue_notify(&self, vq_id: u16) -> std::io::Result<()> {
        self.poller.queue_notify(vq_id)
    }
}

impl VsockBackend for VsockProxy {
    fn queue_notify(&self, queue_id: u16) -> Result<(), VsockError> {
        self.queue_notify(queue_id)
            // Log the raw error in additon to returning the top level
            // `VsockError`
            .inspect_err(|_e| {
                error!(&self.log,
                    "failed to send virtqueue notification";
                    "queue" => %queue_id,
                )
            })
            .map_err(|_| VsockError::QueueNotify { queue: queue_id })
    }
}
