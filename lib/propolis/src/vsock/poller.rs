use std::collections::{HashMap, VecDeque};
use std::ffi::c_void;
use std::io::Read;
use std::mem::MaybeUninit;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::sync::Arc;
use std::thread::JoinHandle;

use slog::{error, Logger};

use crate::hw::virtio::vsock::VsockVq;
use crate::hw::virtio::vsock::VSOCK_RX_QUEUE;
use crate::hw::virtio::vsock::VSOCK_TX_QUEUE;
use crate::vsock::packet::{
    VsockPacket, VIRTIO_VSOCK_OP_CREDIT_REQUEST, VIRTIO_VSOCK_OP_REQUEST,
    VIRTIO_VSOCK_OP_RST, VIRTIO_VSOCK_OP_RW, VIRTIO_VSOCK_OP_SHUTDOWN,
    VIRTIO_VSOCK_SHUTDOWN_F_RECEIVE, VIRTIO_VSOCK_SHUTDOWN_F_SEND,
    VIRTIO_VSOCK_TYPE_STREAM,
};
use crate::vsock::proxy::{ConnKey, VsockProxyConn, CONN_TX_BUF_SIZE};
use crate::vsock::VSOCK_HOST_CID;

enum VsockEvent {
    TxQueue,
    RxQueue,
}

pub struct VsockPollerNotify {
    port_fd: Arc<OwnedFd>,
}

impl VsockPollerNotify {
    fn port_fd(&self) -> BorrowedFd<'_> {
        self.port_fd.as_fd()
    }

    fn port_send(&self, event: VsockEvent) -> std::io::Result<()> {
        let boxed = Box::new(event);
        let user_ptr = Box::into_raw(boxed) as *mut c_void;

        let ret =
            unsafe { libc::port_send(self.port_fd().as_raw_fd(), 0, user_ptr) };

        if ret == 0 {
            Ok(())
        } else {
            // Make sure we don't leak memory from a failed `port_send(3C)`.
            let _ = unsafe { Box::from_raw(user_ptr as *mut VsockEvent) };
            Err(std::io::Error::last_os_error())
        }
    }

    pub fn queue_notify(&self, id: u16) -> std::io::Result<()> {
        match id {
            VSOCK_RX_QUEUE => self.port_send(VsockEvent::RxQueue),
            VSOCK_TX_QUEUE => self.port_send(VsockEvent::TxQueue),
            _ => Ok(()),
        }
    }
}

/// Interest flags for fd registration with event port.
#[derive(Debug, Clone, Copy, Default)]
struct PollInterests(i16);

impl PollInterests {
    const POLLIN: Self = Self(libc::POLLIN);
    const POLLOUT: Self = Self(libc::POLLOUT);
    // const POLLERR: Self = Self(libc::POLLERR);

    fn is_empty(self) -> bool {
        self.0 == 0
    }
}

impl std::ops::BitOr for PollInterests {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self::Output {
        Self(self.0 | rhs.0)
    }
}

impl std::ops::BitOrAssign for PollInterests {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

#[derive(Debug)]
enum RxEvent {
    Reset { src_port: u32, dst_port: u32 },
    NewConnection(ConnKey),
    CreditUpdate(ConnKey),
}

pub struct VsockPoller {
    log: Logger,
    guest_cid: u32,
    port_fd: Arc<OwnedFd>,
    queues: VsockVq,
    connections: HashMap<ConnKey, VsockProxyConn>,
    rx: VecDeque<RxEvent>,
    /// Connections blocked waiting for rx queue descriptors.
    /// These need to be re-registered for POLLIN when space becomes available.
    rx_blocked: Vec<ConnKey>,
}

impl VsockPoller {
    /// Create a new `VsockPoller`.
    ///
    /// This poller is responsible for driving virtio-socket connections between
    /// the guest VM and host sockets.
    pub fn new(
        cid: u32,
        queues: VsockVq,
        log: Logger,
    ) -> std::io::Result<Self> {
        // XXX: set cloexec
        let port_fd = unsafe { libc::port_create() };
        if port_fd == -1 {
            return Err(std::io::Error::last_os_error());
        }

        Ok(Self {
            log,
            guest_cid: cid,
            port_fd: Arc::new(unsafe { OwnedFd::from_raw_fd(port_fd) }),
            queues,
            connections: Default::default(),
            rx: Default::default(),
            rx_blocked: Default::default(),
        })
    }

    /// Get a handle to a `VsockPollerNotify`.
    pub fn notify_handle(&self) -> VsockPollerNotify {
        VsockPollerNotify { port_fd: Arc::clone(&self.port_fd) }
    }

    /// Start the event loop.
    pub fn run(mut self) -> JoinHandle<()> {
        std::thread::Builder::new()
            .name("vsock-event-loop".to_string())
            .spawn(move || self.handle_events())
            .expect("failed to spawn vsock event loop")
    }

    /// Handle a REQUEST packet - create a new connection.
    fn handle_request(&mut self, key: ConnKey, packet: VsockPacket) {
        if self.connections.contains_key(&key) {
            // Connection already exists - send RST
            self.rx.push_back(RxEvent::Reset {
                src_port: key.host_port,
                dst_port: key.guest_port,
            });
            return;
        }

        match VsockProxyConn::new() {
            Ok(mut conn) => {
                conn.update_peer_credit(
                    packet.header.buf_alloc(),
                    packet.header.fwd_cnt(),
                );
                self.connections.insert(key, conn);
                self.rx.push_back(RxEvent::NewConnection(key));
            }
            Err(e) => {
                self.rx.push_back(RxEvent::Reset {
                    src_port: key.host_port,
                    dst_port: key.guest_port,
                });
                error!(self.log, "{e}");
            }
        };
    }

    /// Handle a SHUTDOWN packet - begin graceful close.
    fn handle_shutdown(&mut self, key: ConnKey, flags: u32) {
        if let Some(conn) = self.connections.get_mut(&key) {
            // Guest won't receive more data - stop reading from socket
            if flags & VIRTIO_VSOCK_SHUTDOWN_F_RECEIVE != 0 {
                conn.shutdown_read();
            }
            // Guest won't send more data - stop expecting writes
            if flags & VIRTIO_VSOCK_SHUTDOWN_F_SEND != 0 {
                conn.shutdown_write();
            }
            if conn.should_close() {
                self.connections.remove(&key);
            }
        }
    }

    fn handle_rw_packet(&mut self, key: ConnKey, packet: VsockPacket) {
        if let Some(conn) = self.connections.get_mut(&key) {
            // If we a valid connection attempt to consume the guest's packet.
            conn.recv_packet(packet);

            // Check the connection for:
            // - ring buffered guest packet data
            // - the guest is expecting data from us
            if !conn.has_buffered_data() || !conn.can_write() {
                return;
            }

            let mut interests = PollInterests::POLLOUT;
            if conn.can_read() {
                interests |= PollInterests::POLLIN;
            }

            let fd = conn.get_fd();
            self.associate_fd(key, fd, interests);
        };
    }

    fn handle_tx_queue_event(&mut self) {
        loop {
            let Some(packet) = self.queues.recv_packet() else {
                break;
            };

            let packet = match packet {
                Ok(packet) => packet,
                Err(e) => {
                    eprintln!("{e}");
                    continue;
                }
            };

            // If the packet is not destined for the host drop it.
            if packet.header.dst_cid() != VSOCK_HOST_CID {
                // TODO: info!(self.log, "received packet for...")
                continue;
            }

            let key = ConnKey {
                host_port: packet.header.dst_port(),
                guest_port: packet.header.src_port(),
            };

            // We only support stream connections
            if packet.header.socket_type() != VIRTIO_VSOCK_TYPE_STREAM {
                self.rx.push_back(RxEvent::Reset {
                    src_port: key.host_port,
                    dst_port: key.guest_port,
                });
                // TODO log the packets info
                continue;
            }

            match packet.header.op() {
                VIRTIO_VSOCK_OP_REQUEST => {
                    self.handle_request(key, packet);
                }
                VIRTIO_VSOCK_OP_RST => {
                    self.connections.remove(&key);
                }
                VIRTIO_VSOCK_OP_SHUTDOWN => {
                    self.handle_shutdown(key, packet.header.flags());
                }
                VIRTIO_VSOCK_OP_CREDIT_REQUEST => {
                    if self.connections.contains_key(&key) {
                        self.rx.push_back(RxEvent::CreditUpdate(key));
                    }
                }
                VIRTIO_VSOCK_OP_RW => {
                    self.handle_rw_packet(key, packet);
                }
                // TODO log some info
                _ => {}
            }
        }
    }

    fn handle_rx_queue_event(&mut self) {
        // Now that more descriptors have become available for sending vsock
        // packets attempt to drain pending packets
        self.process_pending_rx();

        // Re-register connections that were blocked waiting for rx queue space
        // XXX: Make this a VeqDec and pop so that we don't drop things on the
        // floor
        let blocked = std::mem::take(&mut self.rx_blocked);
        let to_register: Vec<_> = blocked
            .into_iter()
            .filter_map(|key| {
                self.connections.get(&key).map(|conn| {
                    let fd = conn.get_fd();
                    let mut interests = PollInterests::POLLIN;
                    if conn.has_buffered_data() {
                        interests |= PollInterests::POLLOUT;
                    }
                    (key, fd, interests)
                })
            })
            .collect();

        for (key, fd, interests) in to_register {
            self.associate_fd(key, fd, interests);
        }
    }

    fn process_pending_rx(&mut self) {
        while let Some(rx_event) = self.rx.pop_front() {
            match rx_event {
                RxEvent::Reset { src_port, dst_port } => {
                    self.queues.send_packet(&VsockPacket::new_reset(
                        self.guest_cid,
                        src_port,
                        dst_port,
                    ));
                }
                RxEvent::NewConnection(key) => {
                    self.queues.send_packet(&VsockPacket::new_response(
                        self.guest_cid,
                        key.host_port,
                        key.guest_port,
                        CONN_TX_BUF_SIZE as u32,
                    ));
                    // Now that RESPONSE is sent, register fd for POLLIN
                    if let Some(conn) = self.connections.get(&key) {
                        let fd = conn.get_fd();
                        self.associate_fd(key, fd, PollInterests::POLLIN);
                    }
                }
                RxEvent::CreditUpdate(key) => {
                    if let Some(conn) = self.connections.get_mut(&key) {
                        self.queues.send_packet(
                            &VsockPacket::new_credit_update(
                                self.guest_cid,
                                key.host_port,
                                key.guest_port,
                                conn.buf_alloc(),
                                conn.fwd_cnt(),
                            ),
                        );
                        conn.mark_credit_sent();
                    }
                }
            }
        }
    }

    fn handle_user_event(&mut self, event: PortEvent) {
        let event = unsafe { Box::from_raw(event.user as *mut VsockEvent) };
        match *event {
            VsockEvent::TxQueue => self.handle_tx_queue_event(),
            VsockEvent::RxQueue => self.handle_rx_queue_event(),
        }
    }

    fn handle_fd_event(&mut self, event: PortEvent, read_buf: &mut [u8]) {
        let key = ConnKey::from_usize(event.user);
        let fd = event.object as RawFd;

        if event.events & i32::from(libc::POLLOUT) != 0 {
            self.handle_pollout(key, fd);
        }

        if event.events & i32::from(libc::POLLIN) != 0 {
            self.handle_pollin(key, fd, read_buf);
        }
    }

    /// Handle POLLOUT - drain buffered guest data to the host socket.
    fn handle_pollout(&mut self, key: ConnKey, fd: RawFd) {
        let VsockPoller { queues, connections, guest_cid, .. } = self;

        let Some(conn) = connections.get_mut(&key) else {
            return;
        };

        loop {
            match conn.flush() {
                Ok(0) => break,
                Ok(nbytes) => {
                    conn.update_fwd_cnt(nbytes as u32);
                    if conn.needs_credit_update() {
                        conn.mark_credit_sent();
                        queues.send_packet(&VsockPacket::new_credit_update(
                            *guest_cid,
                            key.host_port,
                            key.guest_port,
                            conn.buf_alloc(),
                            conn.fwd_cnt(),
                        ));
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(e) => {
                    eprintln!("error writing to socket: {e}");
                    break;
                }
            }
        }

        // Check for close or re-register
        if conn.should_close() {
            connections.remove(&key);
            return;
        }

        let mut interests = PollInterests::default();
        if conn.can_read() {
            interests |= PollInterests::POLLIN;
        }
        if conn.has_buffered_data() && conn.can_write() {
            interests |= PollInterests::POLLOUT;
        }
        if !interests.is_empty() {
            self.associate_fd(key, fd, interests);
        }
    }

    /// Handle POLLIN - read from host socket and send to guest.
    fn handle_pollin(&mut self, key: ConnKey, fd: RawFd, read_buf: &mut [u8]) {
        let VsockPoller { queues, connections, guest_cid, rx_blocked, .. } =
            self;

        let Some(conn) = connections.get_mut(&key) else {
            return;
        };

        if !conn.can_read() {
            return;
        }

        let mut rx_queue_blocked = false;

        loop {
            let Some(permit) = queues.try_rx_permit() else {
                rx_queue_blocked = true;
                break;
            };

            let credit = conn.peer_credit();
            if credit == 0 {
                break;
            }

            let max_read = std::cmp::min(
                permit.available_data_space(),
                std::cmp::min(credit as usize, read_buf.len()),
            );

            match conn.socket.read(&mut read_buf[..max_read]) {
                Ok(0) => {
                    // EOF from host socket - graceful close
                    conn.shutdown_read();
                    queues.send_packet(&VsockPacket::new_shutdown(
                        *guest_cid,
                        key.host_port,
                        key.guest_port,
                        VIRTIO_VSOCK_SHUTDOWN_F_SEND,
                    ));
                    break;
                }
                Ok(nbytes) => {
                    conn.update_tx_cnt(nbytes as u32);
                    permit.write_rw(
                        *guest_cid,
                        key.host_port,
                        key.guest_port,
                        conn.buf_alloc(),
                        conn.fwd_cnt(),
                        &read_buf[..nbytes],
                    );
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(_e) => {
                    // Socket error - send RST to indicate abnormal termination
                    connections.remove(&key);
                    queues.send_packet(&VsockPacket::new_reset(
                        *guest_cid,
                        key.host_port,
                        key.guest_port,
                    ));
                    return;
                }
            }
        }

        // Check for close or re-register
        if conn.should_close() {
            connections.remove(&key);
            return;
        }

        if rx_queue_blocked {
            rx_blocked.push(key);
        }

        let mut interests = PollInterests::default();
        if conn.can_read() && !rx_queue_blocked {
            interests |= PollInterests::POLLIN;
        }
        if conn.has_buffered_data() && conn.can_write() {
            interests |= PollInterests::POLLOUT;
        }
        if !interests.is_empty() {
            self.associate_fd(key, fd, interests);
        }
    }

    fn associate_fd(
        &mut self,
        key: ConnKey,
        fd: RawFd,
        interests: PollInterests,
    ) {
        let ret = unsafe {
            libc::port_associate(
                self.port_fd.as_raw_fd(),
                libc::PORT_SOURCE_FD,
                fd as usize,
                interests.0.into(),
                key.to_usize() as *mut c_void,
            )
        };

        if ret < 0 {
            panic!(
                "failed port associate: {}",
                std::io::Error::last_os_error()
            );
        }
    }

    fn handle_events(&mut self) {
        const MAX_EVENTS: u32 = 32;

        let mut events: [MaybeUninit<libc::port_event>; MAX_EVENTS as usize] =
            [const { MaybeUninit::uninit() }; MAX_EVENTS as usize];
        let mut read_buf = vec![0u8; 1500];

        loop {
            let mut nget = 1;

            let ret = unsafe {
                libc::port_getn(
                    self.port_fd.as_raw_fd(),
                    events.as_mut_ptr() as *mut libc::port_event,
                    MAX_EVENTS,
                    &mut nget,
                    std::ptr::null_mut(),
                )
            };

            if ret < 0 {
                todo!("handle the errors")
            }

            for i in 0..nget as usize {
                let event = PortEvent::from_raw(unsafe {
                    events[i].assume_init_read()
                });
                match event.source {
                    EventSource::User => self.handle_user_event(event),
                    EventSource::Fd => {
                        self.handle_fd_event(event, &mut read_buf)
                    }
                    _ => {}
                }
            }

            // Process any pending rx events
            self.process_pending_rx();
        }
    }
}

/// The source of a port event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventSource {
    /// User event i.e. `port_send(3C)`
    User,
    /// File descriptor event
    Fd,
    /// Unknown source for the vsock backend
    Unknown(u16),
}

impl EventSource {
    fn from_raw(source: u16) -> Self {
        match source as i32 {
            libc::PORT_SOURCE_USER => EventSource::User,
            libc::PORT_SOURCE_FD => EventSource::Fd,
            _ => EventSource::Unknown(source),
        }
    }
}

/// A port event retrieved from an event port.
///
/// This represents an event from one of the various event sources (file
/// descriptors, timers, user events, etc.).
#[derive(Debug, Clone)]
pub struct PortEvent {
    /// The events that occurred (source-specific)
    pub events: i32,
    /// The source of the event
    pub source: EventSource,
    /// The object associated with the event (interpretation depends on source)
    pub object: usize,
    /// User-defined data provided during association
    pub user: usize,
}

impl PortEvent {
    fn from_raw(event: libc::port_event) -> Self {
        PortEvent {
            events: event.portev_events,
            source: EventSource::from_raw(event.portev_source),
            object: event.portev_object,
            user: event.portev_user as usize,
        }
    }
}
