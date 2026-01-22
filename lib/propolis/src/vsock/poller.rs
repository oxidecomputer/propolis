use std::collections::{HashMap, VecDeque};
use std::ffi::c_void;
use std::io::{ErrorKind, Read};
use std::mem::MaybeUninit;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::sync::Arc;
use std::thread::JoinHandle;

use slog::{error, warn, Logger};

use crate::hw::virtio::vsock::VsockVq;
use crate::hw::virtio::vsock::VSOCK_RX_QUEUE;
use crate::hw::virtio::vsock::VSOCK_TX_QUEUE;
use crate::vsock::packet::{
    VsockPacket, VsockPacketHeader, VIRTIO_VSOCK_OP_CREDIT_REQUEST,
    VIRTIO_VSOCK_OP_CREDIT_UPDATE, VIRTIO_VSOCK_OP_REQUEST,
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
        let port_fd = unsafe {
            let fd = match libc::port_create() {
                -1 => return Err(std::io::Error::last_os_error()),
                fd => fd,
            };

            // Set CLOEXEC on the event port fd
            if libc::fcntl(
                fd,
                libc::F_SETFD,
                libc::fcntl(fd, libc::F_GETFD) | libc::FD_CLOEXEC,
            ) < 0
            {
                return Err(std::io::Error::last_os_error());
            };

            fd
        };

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

    /// Handle the guest's VIRTIO_VSOCK_OP_REQUEST packet.
    fn handle_connection_request(&mut self, key: ConnKey, packet: VsockPacket) {
        if self.connections.contains_key(&key) {
            // Connection already exists
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

    /// Handle the guest's VIRTIO_VSOCK_OP_SHUTDOWN packet.
    fn handle_shutdown(&mut self, key: ConnKey, flags: u32) {
        if let Some(conn) = self.connections.get_mut(&key) {
            // Guest won't receive more data
            if flags & VIRTIO_VSOCK_SHUTDOWN_F_RECEIVE != 0 {
                conn.shutdown_read().unwrap();
            }
            // Guest won't send more data
            if flags & VIRTIO_VSOCK_SHUTDOWN_F_SEND != 0 {
                conn.shutdown_write().unwrap();
            }
            // XXX how do we register this for future cleanup if there is data
            // we have not synced locally yet? We need a cleanup loop...
            if conn.should_close() {
                if !conn.has_buffered_data() {
                    self.connections.remove(&key);
                    // virtio spec states:
                    //
                    // Clean disconnect is achieved by one or more
                    // VIRTIO_VSOCK_OP_SHUTDOWN packets that indicate no more data
                    // will be sent and received, followed by a VIRTIO_VSOCK_OP_RST
                    // response from the peer.
                    self.rx.push_back(RxEvent::Reset {
                        src_port: key.host_port,
                        dst_port: key.guest_port,
                    });
                }
            }
        }
    }

    /// Handle the guest's VIRTIO_VSOCK_OP_RW packet.
    fn handle_rw_packet(&mut self, key: ConnKey, packet: VsockPacket) {
        if let Some(conn) = self.connections.get_mut(&key) {
            // If we a valid connection attempt to consume the guest's packet.
            conn.recv_packet(packet);

            // If we have buffered data, register for POLLOUT to flush it.
            if !conn.has_buffered_data() {
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
            let packet = match self.queues.recv_packet().transpose() {
                Ok(Some(packet)) => packet,
                // No more packets on the guests tx queue
                Ok(None) => break,
                Err(e) => {
                    warn!(&self.log, "dropping invalid vsock packet: {e}");
                    continue;
                }
            };

            // If the packet is not destined for the host drop it.
            if packet.header.dst_cid() != VSOCK_HOST_CID {
                warn!(
                    &self.log,
                    "droppping vsock packet not destined for the host";
                    "packet" => ?packet,
                );
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
                warn!(&self.log,
                    "received invalid vsock packet";
                    "type" => %packet.header.socket_type(),
                );
                continue;
            }

            if let Some(conn) = self.connections.get_mut(&key) {
                // Regardless of the vsock operation we need to record the peers
                // credit info
                conn.update_peer_credit(
                    packet.header.buf_alloc(),
                    packet.header.fwd_cnt(),
                );
                match packet.header.op() {
                    VIRTIO_VSOCK_OP_RST => {
                        self.connections.remove(&key);
                    }
                    VIRTIO_VSOCK_OP_SHUTDOWN => {
                        self.handle_shutdown(key, packet.header.flags());
                    }
                    // Handled above for every packet
                    VIRTIO_VSOCK_OP_CREDIT_UPDATE => continue,
                    VIRTIO_VSOCK_OP_CREDIT_REQUEST => {
                        if self.connections.contains_key(&key) {
                            self.rx.push_back(RxEvent::CreditUpdate(key));
                        }
                    }
                    VIRTIO_VSOCK_OP_RW => {
                        self.handle_rw_packet(key, packet);
                    }
                    _ => {
                        warn!(
                            &self.log,
                            "received vsock packet with unknown op code";
                            "packet" => ?packet,
                        );
                    }
                }
            } else {
                match packet.header.op() {
                    VIRTIO_VSOCK_OP_REQUEST => {
                        self.handle_connection_request(key, packet)
                    }
                    // VIRTIO_VSOCK_OP_RST => {}
                    _ => {
                        warn!(
                            &self.log,
                            "received a vsock packet for an unknown connection \
                            that was not a REQUEST or RST";
                            "packet" => ?packet,
                        );
                    }
                }
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
        while let Some(permit) = self.queues.try_rx_permit() {
            let Some(rx_event) = self.rx.pop_front() else {
                break;
            };

            match rx_event {
                RxEvent::Reset { src_port, dst_port } => {
                    let packet = VsockPacket::new_reset(
                        self.guest_cid,
                        src_port,
                        dst_port,
                    );
                    permit.write(&packet.header, &packet.data);
                }
                RxEvent::NewConnection(key) => {
                    let packet = VsockPacket::new_response(
                        self.guest_cid,
                        key.host_port,
                        key.guest_port,
                        CONN_TX_BUF_SIZE as u32,
                    );
                    permit.write(&packet.header, &packet.data);

                    if let Some(conn) = self.connections.get_mut(&key) {
                        conn.set_established().unwrap();
                        let fd = conn.get_fd();
                        self.associate_fd(key, fd, PollInterests::POLLIN);
                    }
                }
                RxEvent::CreditUpdate(key) => {
                    if let Some(conn) = self.connections.get_mut(&key) {
                        let packet = VsockPacket::new_credit_update(
                            self.guest_cid,
                            key.host_port,
                            key.guest_port,
                            conn.buf_alloc(),
                            conn.fwd_cnt(),
                        );
                        permit.write(&packet.header, &packet.data);
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
        let Some(conn) = self.connections.get_mut(&key) else {
            return;
        };

        loop {
            match conn.flush() {
                Ok(0) => break,
                Ok(nbytes) => {
                    conn.update_fwd_cnt(nbytes as u32);
                    if conn.needs_credit_update() {
                        self.rx.push_back(RxEvent::CreditUpdate(key));
                    }
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                Err(e) => {
                    eprintln!("error writing to socket: {e}");
                    break;
                }
            }
        }

        // We have finished draining our buffered data to the host, so check if
        // we should remove ourselves from the active connections.
        if conn.should_close() {
            self.connections.remove(&key);
            return;
        }

        let mut interests = PollInterests::default();
        if conn.can_read() {
            interests |= PollInterests::POLLIN;
        }
        if conn.has_buffered_data() {
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

        // The guest is no longer expecting any data
        if !conn.can_read() {
            return;
        }

        loop {
            let Some(permit) = queues.try_rx_permit() else {
                dbg!("blocked");
                rx_blocked.push(key);
                break;
            };

            let credit = conn.peer_credit();
            if credit == 0 {
                // XXX when this happens we need to not sit in a tight loop
                dbg!("no credit");
                break;
            }

            let max_read = std::cmp::min(
                permit.available_data_space(),
                std::cmp::min(credit as usize, read_buf.len()),
            );

            match conn.socket.read(&mut read_buf[..max_read]) {
                Ok(0) => {
                    let packet = VsockPacket::new_shutdown(
                        *guest_cid,
                        key.host_port,
                        key.guest_port,
                        VIRTIO_VSOCK_SHUTDOWN_F_SEND,
                    );
                    permit.write(&packet.header, &packet.data);
                    return;
                }
                Ok(nbytes) => {
                    conn.update_tx_cnt(nbytes as u32);
                    let mut header = VsockPacketHeader::default();
                    header
                        .set_src_cid(VSOCK_HOST_CID as u32)
                        .set_dst_cid(*guest_cid)
                        .set_src_port(key.host_port)
                        .set_dst_port(key.guest_port)
                        .set_len(nbytes as u32)
                        .set_socket_type(VIRTIO_VSOCK_TYPE_STREAM)
                        .set_op(VIRTIO_VSOCK_OP_RW)
                        .set_buf_alloc(conn.buf_alloc())
                        .set_fwd_cnt(conn.fwd_cnt());
                    permit.write(&header, &read_buf[..nbytes]);
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                Err(_e) => {
                    connections.remove(&key);
                    let packet = VsockPacket::new_reset(
                        *guest_cid,
                        key.host_port,
                        key.guest_port,
                    );
                    permit.write(&packet.header, &packet.data);
                    return;
                }
            }
        }

        // XXX why check this here we got woken up for POLLIN
        // Check for close or re-register
        // if conn.should_close() {
        //     // XXX we need to send a RST according to the spec?
        //     connections.remove(&key);
        //     return;
        // }

        let mut interests = PollInterests::default();
        if conn.can_read() {
            interests |= PollInterests::POLLIN;
        }
        if conn.has_buffered_data() {
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

            // XXX consider a timeout so that we can perform other cleanup type
            // actions?
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
                let err = std::io::Error::last_os_error();
                // SAFETY: The docs state that `raw_os_error` will always return
                // a `Some` variant when obtained via `las_os_error`.
                match err.raw_os_error().unwrap() {
                    // A signal was caught so process the loop again
                    libc::EINTR => continue,
                    libc::EBADF => {
                        // XXX This means our event loop is effectively no
                        // longer servicable and the vsock device is useless.
                        // Can we attempt to recover from this?
                        error!(
                            &self.log,
                            "vsock port fd is no longer valid: {err}"
                        );
                        break;
                    }
                    _ => {
                        error!(&self.log, "vsock port_getn returned: {err}");
                        continue;
                    }
                }
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
struct PortEvent {
    /// The events that occurred (source-specific)
    events: i32,
    /// The source of the event
    source: EventSource,
    /// The object associated with the event (interpretation depends on source)
    object: usize,
    /// User-defined data provided during association
    user: usize,
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
