use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::collections::VecDeque;
use std::ffi::c_void;
use std::io::ErrorKind;
use std::io::Read;
use std::mem::MaybeUninit;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::sync::Arc;
use std::thread::JoinHandle;

use bitflags::bitflags;
use iddqd::IdHashMap;
use slog::{debug, error, info, warn, Logger};

use crate::hw::virtio::vsock::VsockVq;
use crate::hw::virtio::vsock::VSOCK_RX_QUEUE;
use crate::hw::virtio::vsock::VSOCK_TX_QUEUE;
use crate::vsock::packet::VsockPacket;
use crate::vsock::packet::VsockPacketHeader;
use crate::vsock::packet::VIRTIO_VSOCK_OP_CREDIT_REQUEST;
use crate::vsock::packet::VIRTIO_VSOCK_OP_CREDIT_UPDATE;
use crate::vsock::packet::VIRTIO_VSOCK_OP_REQUEST;
use crate::vsock::packet::VIRTIO_VSOCK_OP_RST;
use crate::vsock::packet::VIRTIO_VSOCK_OP_RW;
use crate::vsock::packet::VIRTIO_VSOCK_OP_SHUTDOWN;
use crate::vsock::packet::VIRTIO_VSOCK_SHUTDOWN_F_RECEIVE;
use crate::vsock::packet::VIRTIO_VSOCK_SHUTDOWN_F_SEND;
use crate::vsock::packet::VIRTIO_VSOCK_TYPE_STREAM;
use crate::vsock::proxy::ConnKey;
use crate::vsock::proxy::VsockPortMapping;
use crate::vsock::proxy::VsockProxyConn;
use crate::vsock::proxy::CONN_TX_BUF_SIZE;
use crate::vsock::VSOCK_HOST_CID;

#[repr(usize)]
enum VsockEvent {
    TxQueue = 0,
    RxQueue,
    Shutdown,
}

pub struct VsockPollerNotify {
    port_fd: Arc<OwnedFd>,
}

impl VsockPollerNotify {
    fn port_fd(&self) -> BorrowedFd<'_> {
        self.port_fd.as_fd()
    }

    fn port_send(&self, event: VsockEvent) -> std::io::Result<()> {
        let ret = unsafe {
            libc::port_send(self.port_fd().as_raw_fd(), 0, event as usize as _)
        };

        if ret == 0 {
            Ok(())
        } else {
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

    pub fn shutdown(&self) -> std::io::Result<()> {
        self.port_send(VsockEvent::Shutdown)
    }
}

bitflags! {
    #[repr(transparent)]
    #[derive(Copy, Clone, Eq, PartialEq, Hash, Debug)]
    struct PollEvents: i32 {
        const IN = libc::POLLIN as i32;
        const PRI = libc::POLLPRI as i32;
        const OUT = libc::POLLOUT as i32;
        const ERR = libc::POLLERR as i32;
        const HUP = libc::POLLHUP as i32;
    }
}

/// Set of `PollEvents` that signifies a readable event.
fn fd_readable() -> PollEvents {
    PollEvents::IN | PollEvents::HUP | PollEvents::ERR | PollEvents::PRI
}

/// Set of `PollEvents` that signifies a writable event.
fn fd_writable() -> PollEvents {
    PollEvents::OUT | PollEvents::HUP | PollEvents::ERR
}

#[derive(Debug)]
enum RxEvent {
    Reset(ConnKey),
    NewConnection(ConnKey),
    CreditUpdate(ConnKey),
}

pub struct VsockPoller {
    log: Logger,
    guest_cid: u32,
    port_mappings: IdHashMap<VsockPortMapping>,
    port_fd: Arc<OwnedFd>,
    queues: VsockVq,
    connections: HashMap<ConnKey, VsockProxyConn>,
    rx: VecDeque<RxEvent>,
    /// Connections blocked waiting for rx queue descriptors.
    /// These need to be re-registered for `PollEvents::IN` when space becomes
    /// available.
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
        port_mappings: IdHashMap<VsockPortMapping>,
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

        info!(
            &log,
            "vsock poller configured with";
            "mappings" => ?port_mappings,
        );

        Ok(Self {
            log,
            guest_cid: cid,
            port_mappings,
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
            self.send_conn_rst(key);
            return;
        }

        let Some(mapping) = self.port_mappings.get(&packet.header.dst_port())
        else {
            // Drop the unknown connection so that it times out in the guest.
            debug!(
                &self.log,
                "dropping connect request to unknown mapping";
                "packet" => ?packet,
            );
            return;
        };

        match VsockProxyConn::new(mapping.addr()) {
            Ok(mut conn) => {
                conn.update_peer_credit(&packet.header);
                self.connections.insert(key, conn);
                self.rx.push_back(RxEvent::NewConnection(key));
            }
            Err(e) => {
                self.send_conn_rst(key);
                error!(self.log, "{e}");
            }
        };
    }

    /// Handle the guest's VIRTIO_VSOCK_OP_SHUTDOWN packet.
    fn handle_shutdown(&mut self, key: ConnKey, flags: u32) {
        if let Entry::Occupied(mut entry) = self.connections.entry(key) {
            let conn = entry.get_mut();

            // Guest won't receive more data
            if flags & VIRTIO_VSOCK_SHUTDOWN_F_RECEIVE != 0 {
                if let Err(e) = conn.shutdown_read() {
                    error!(
                        &self.log,
                        "cannot transition vsock connection state: {e}";
                        "conn" => ?conn,
                    );
                    entry.remove();
                    self.send_conn_rst(key);
                    return;
                };
            }
            // Guest won't send more data
            if flags & VIRTIO_VSOCK_SHUTDOWN_F_SEND != 0 {
                if let Err(e) = conn.shutdown_write() {
                    error!(
                        &self.log,
                        "cannot transition vsock connection state: {e}";
                        "conn" => ?conn,
                    );
                    entry.remove();
                    self.send_conn_rst(key);
                    return;
                };
            }
            // XXX how do we register this for future cleanup if there is data
            // we have not synced locally yet? We need a cleanup loop...
            if conn.should_close() {
                if !conn.has_buffered_data() {
                    self.connections.remove(&key);
                    // virtio spec states:
                    //
                    // Clean disconnect is achieved by one or more
                    // VIRTIO_VSOCK_OP_SHUTDOWN packets that indicate no
                    // more data will be sent and received, followed by a
                    // VIRTIO_VSOCK_OP_RST response from the peer.
                    self.send_conn_rst(key);
                }
            }
        }
    }

    /// Handle the guest's VIRTIO_VSOCK_OP_RW packet.
    fn handle_rw_packet(&mut self, key: ConnKey, packet: VsockPacket) {
        if let Some(conn) = self.connections.get_mut(&key) {
            // If we a valid connection attempt to consume the guest's packet.
            conn.recv_packet(packet);

            let mut interests = PollEvents::empty();
            interests.set(PollEvents::OUT, conn.has_buffered_data());
            interests.set(PollEvents::IN, conn.can_read());
            if !interests.is_empty() {
                let fd = conn.get_fd();
                self.associate_fd(key, fd, interests);
            }
        };
    }

    /// Handle the guest's tx virtqueue.
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
                debug!(
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
                self.send_conn_rst(key);
                warn!(&self.log,
                    "received invalid vsock packet";
                    "type" => %packet.header.socket_type(),
                );
                continue;
            }

            if let Some(conn) = self.connections.get_mut(&key) {
                // Regardless of the vsock operation we need to record the peers
                // credit info
                conn.update_peer_credit(&packet.header);
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
                    VIRTIO_VSOCK_OP_RST => {}
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

        // Re-register connections that were blocked waiting for rx queue space.
        // It would be nice if we had a hint of how many descriptors became
        // available but that's not the case today.
        for key in std::mem::take(&mut self.rx_blocked).drain(..) {
            if let Some(conn) = self.connections.get(&key) {
                let fd = conn.get_fd();
                let mut interests = PollEvents::IN;
                interests.set(PollEvents::OUT, conn.has_buffered_data());
                self.associate_fd(key, fd, interests);
            }
        }
    }

    fn process_pending_rx(&mut self) {
        while let Some(permit) = self.queues.try_rx_permit() {
            let Some(rx_event) = self.rx.pop_front() else {
                break;
            };

            match rx_event {
                RxEvent::Reset(key) => {
                    let packet = VsockPacket::new_reset(
                        self.guest_cid,
                        key.host_port,
                        key.guest_port,
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

                    if let Entry::Occupied(mut entry) =
                        self.connections.entry(key)
                    {
                        let conn = entry.get_mut();
                        if let Err(e) = conn.set_established() {
                            error!(
                                &self.log,
                                "cannot transition vsock connection state: {e}";
                                "conn" => ?conn,
                            );
                            entry.remove();
                            self.send_conn_rst(key);
                            continue;
                        };

                        let fd = conn.get_fd();
                        self.associate_fd(key, fd, PollEvents::IN);
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

    /// Handle a user event. Returns `true` if the event loop should shut down.
    fn handle_user_event(&mut self, event: PortEvent) -> bool {
        match event.user {
            val if val == VsockEvent::TxQueue as usize => {
                self.handle_tx_queue_event()
            }
            val if val == VsockEvent::RxQueue as usize => {
                self.handle_rx_queue_event()
            }
            val if val == VsockEvent::Shutdown as usize => return true,
            _ => (),
        }
        false
    }

    fn handle_fd_event(&mut self, event: PortEvent, read_buf: &mut [u8]) {
        let key = ConnKey::from_portev_user(event.user);
        let events = PollEvents::from_bits_retain(event.events);

        if fd_writable().intersects(events) {
            self.handle_writable_fd(key);
        }

        if fd_readable().intersects(events) {
            self.handle_readable_fd(key, read_buf);
        }
    }

    /// When an fd is writable, drain buffered guest data to the host socket.
    fn handle_writable_fd(&mut self, key: ConnKey) {
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
        if conn.should_close() && !conn.has_buffered_data() {
            // XXX do we need to send a reset?
            self.connections.remove(&key);
            return;
        }

        let mut interests = PollEvents::empty();
        interests.set(PollEvents::OUT, conn.has_buffered_data());
        interests.set(PollEvents::IN, conn.can_read());
        if !interests.is_empty() {
            let fd = conn.get_fd();
            self.associate_fd(key, fd, interests);
        }
    }

    /// When an fd is readable, read from host socket and send to guest.
    fn handle_readable_fd(&mut self, key: ConnKey, read_buf: &mut [u8]) {
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
                Err(e) => {
                    error!(
                        &self.log,
                        "vsock backend socket read faild: {e}";
                        "key" => ?key,
                        "conn" => ?conn,
                    );

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

        let mut interests = PollEvents::empty();
        interests.set(PollEvents::OUT, conn.has_buffered_data());
        interests.set(PollEvents::IN, conn.can_read());
        if !interests.is_empty() {
            let fd = conn.get_fd();
            self.associate_fd(key, fd, interests);
        }
    }

    fn associate_fd(&mut self, key: ConnKey, fd: RawFd, interests: PollEvents) {
        let ret = unsafe {
            libc::port_associate(
                self.port_fd.as_raw_fd(),
                libc::PORT_SOURCE_FD,
                fd as usize,
                interests.bits(),
                key.to_portev_user() as *mut c_void,
            )
        };

        if ret < 0 {
            let err = std::io::Error::last_os_error();
            if let Some(conn) = self.connections.remove(&key) {
                error!(
                    &self.log,
                    "vsock port_assocaite failed: {err}";
                    "key" => ?key,
                    "conn" => ?conn,
                );
                self.send_conn_rst(key);
            }
        }
    }

    /// Enqueue a RST packet for the provided [`ConnKey`]
    fn send_conn_rst(&mut self, key: ConnKey) {
        self.rx.push_back(RxEvent::Reset(key));
    }

    fn handle_events(&mut self) {
        const MAX_EVENTS: u32 = 32;

        let mut events: [MaybeUninit<libc::port_event>; MAX_EVENTS as usize] =
            [const { MaybeUninit::uninit() }; MAX_EVENTS as usize];
        let mut read_buf = vec![0u8; 1024 * 64];

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
                let err = std::io::Error::last_os_error();
                // SAFETY: The docs state that `raw_os_error` will always return
                // a `Some` variant when obtained via `las_os_error`.
                match err.raw_os_error().unwrap() {
                    // A signal was caught so process the loop again
                    libc::EINTR => continue,
                    libc::EBADF | libc::EBADFD => {
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
                    EventSource::User => {
                        let should_shutdown = self.handle_user_event(event);
                        if should_shutdown {
                            return;
                        }
                    }
                    EventSource::Fd => {
                        self.handle_fd_event(event, &mut read_buf);
                    }
                    _ => {}
                };
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
    #[allow(dead_code)]
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

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    use iddqd::IdHashMap;
    use zerocopy::FromBytes;

    use crate::accessors::MemAccessor;
    use crate::common::GuestAddr;
    use crate::hw::virtio::testutil::{DescFlag, VirtQueues, VqSize};
    use crate::hw::virtio::vsock::VsockVq;
    use crate::vmm::mem::PhysMap;
    use crate::vsock::packet::{
        VsockPacketHeader, VIRTIO_VSOCK_OP_CREDIT_UPDATE,
        VIRTIO_VSOCK_OP_REQUEST, VIRTIO_VSOCK_OP_RESPONSE, VIRTIO_VSOCK_OP_RST,
        VIRTIO_VSOCK_OP_RW, VIRTIO_VSOCK_OP_SHUTDOWN,
        VIRTIO_VSOCK_SHUTDOWN_F_SEND, VIRTIO_VSOCK_TYPE_STREAM,
    };
    use crate::vsock::proxy::{VsockPortMapping, CONN_TX_BUF_SIZE};
    use crate::vsock::VSOCK_HOST_CID;

    use super::VsockPoller;

    fn test_logger() -> slog::Logger {
        use slog::Drain;
        let decorator = slog_term::TermDecorator::new().stderr().build();
        let drain = slog_term::FullFormat::new(decorator).build().fuse();
        let drain = slog_async::Async::new(drain).build().fuse();
        slog::Logger::root(drain, slog::o!("component" => "vsock-test"))
    }

    const QUEUE_SIZE: u16 = 64;
    const PAGE_SIZE: u64 = 0x1000;

    /// Bind a TCP listener on an ephemeral port and return it along with an
    /// `IdHashMap<BackendListener>` that maps `vsock_port` to the listener's
    /// actual address.
    fn bind_test_backend(
        vsock_port: u32,
    ) -> (TcpListener, IdHashMap<VsockPortMapping>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let mut backends = IdHashMap::new();
        backends.insert_overwrite(VsockPortMapping::new(vsock_port, addr));
        (listener, backends)
    }

    const fn align_up(val: u64, align: u64) -> u64 {
        (val + align - 1) & !(align - 1)
    }

    /// 16-byte virtio descriptor, matching the on-wire/in-memory layout.
    #[repr(C)]
    #[derive(Copy, Clone, Default, FromBytes)]
    struct RawDesc {
        addr: u64,
        len: u32,
        flags: u16,
        next: u16,
    }

    /// Guest physical address layout for a single virtqueue's ring structures.
    #[derive(Copy, Clone)]
    struct QueueLayout {
        desc_base: u64,
        avail_base: u64,
        used_base: u64,
        /// First GPA after this queue's structures (start of next region).
        end: u64,
    }

    impl QueueLayout {
        /// Compute the ring layout for a queue of `size` entries starting
        /// at `base`.
        fn new(base: u64, size: u16) -> Self {
            let qsz = size as u64;
            let desc_base = base;
            let avail_base = desc_base + 16 * qsz;
            let used_base = align_up(avail_base + 4 + 2 * qsz, PAGE_SIZE);
            let end = align_up(used_base + 4 + 8 * qsz, PAGE_SIZE);
            Self { desc_base, avail_base, used_base, end }
        }
    }

    /// Test guest memory and queue layout produced by [`make_test_vsock_vq`].
    ///
    /// The `VsockVq` is returned separately so it can be moved into the
    /// `VsockPoller` while this struct remains borrowed.
    struct TestVsockVq {
        mem_acc: MemAccessor,
        /// Must stay alive to keep memory mappings valid.
        _phys: PhysMap,
        /// The underlying VirtQueues — must stay alive to keep queue state valid.
        _raw_queues: VirtQueues,
        rx: QueueLayout,
        tx: QueueLayout,
        event: QueueLayout,
    }

    /// Build a VsockVq backed by test guest memory with 3 queues
    /// (RX, TX, EVENT).
    fn make_test_vsock_vq() -> (VsockVq, TestVsockVq) {
        // Lay out 3 queues sequentially
        let rx = QueueLayout::new(0, QUEUE_SIZE);
        let tx = QueueLayout::new(rx.end, QUEUE_SIZE);
        let event = QueueLayout::new(tx.end, 1);

        // Data area after all rings — sized generously to support tests that
        // send large amounts of data (e.g. credit update tests with >64KB).
        let data_area_size = PAGE_SIZE * 64;
        let total_size =
            align_up(event.end + data_area_size, PAGE_SIZE) as usize;

        let mut phys = PhysMap::new_test(total_size);
        phys.add_test_mem("vsock-test".to_string(), 0, total_size)
            .expect("add test mem");
        let mem_acc = phys.finalize();

        // Create 3 queues via VirtQueues
        let raw_queues = VirtQueues::new(&[
            VqSize::new(QUEUE_SIZE),
            VqSize::new(QUEUE_SIZE),
            VqSize::new(1),
        ]);

        let layouts = [rx, tx, event];
        for (i, layout) in layouts.iter().enumerate() {
            let vq = raw_queues.get(i as u16).unwrap();
            mem_acc.adopt(&vq.acc_mem, Some(format!("test-vq-{i}")));
            vq.map_virtqueue(
                layout.desc_base,
                layout.avail_base,
                layout.used_base,
            );
            vq.live.store(true, std::sync::atomic::Ordering::Release);
            vq.enabled.store(true, std::sync::atomic::Ordering::Release);

            // Zero out avail and used ring headers
            let mem = mem_acc.access().unwrap();
            mem.write(GuestAddr(layout.avail_base), &0u16);
            mem.write(GuestAddr(layout.avail_base + 2), &0u16);
            mem.write(GuestAddr(layout.used_base), &0u16);
            mem.write(GuestAddr(layout.used_base + 2), &0u16);
        }

        let vq_arcs: Vec<_> =
            (0..3).map(|i| raw_queues.get(i).unwrap().clone()).collect();
        let vsock_acc = mem_acc.child(Some("vsock-vq-acc".to_string()));
        let vq = VsockVq::new(vq_arcs, vsock_acc);

        (
            vq,
            TestVsockVq {
                mem_acc,
                _phys: phys,
                _raw_queues: raw_queues,
                rx,
                tx,
                event,
            },
        )
    }

    /// State for injecting descriptors into a specific queue's rings.
    struct QueueWriter {
        layout: QueueLayout,
        /// Next free descriptor index.
        next_desc: u16,
        /// Next free data area offset (GPA).
        data_cursor: u64,
        /// Avail ring index we've published up to.
        avail_idx: u16,
    }

    impl QueueWriter {
        fn new(layout: QueueLayout, data_start: u64) -> Self {
            Self { layout, next_desc: 0, data_cursor: data_start, avail_idx: 0 }
        }

        /// Write a descriptor and return its index.
        fn write_desc(
            &mut self,
            mem_acc: &MemAccessor,
            addr: u64,
            len: u32,
            flags: u16,
            next: u16,
        ) -> u16 {
            let idx = self.next_desc;
            self.next_desc += 1;
            let desc = RawDesc { addr, len, flags, next };
            let gpa = self.layout.desc_base + u64::from(idx) * 16;
            let mem = mem_acc.access().unwrap();
            mem.write(GuestAddr(gpa), &desc);
            idx
        }

        /// Allocate data space and write bytes into it. Returns the GPA.
        fn write_data(&mut self, mem_acc: &MemAccessor, data: &[u8]) -> u64 {
            let gpa = self.data_cursor;
            self.data_cursor += data.len() as u64;
            let mem = mem_acc.access().unwrap();
            mem.write_from(GuestAddr(gpa), data, data.len());
            gpa
        }

        /// Allocate data space without writing. Returns the GPA.
        fn alloc_data(&mut self, len: u32) -> u64 {
            let gpa = self.data_cursor;
            self.data_cursor += u64::from(len);
            gpa
        }

        /// Add a readable descriptor with the given data.
        fn add_readable(&mut self, mem_acc: &MemAccessor, data: &[u8]) -> u16 {
            let gpa = self.write_data(mem_acc, data);
            self.write_desc(
                mem_acc,
                gpa,
                data.len() as u32,
                0, // readable
                0,
            )
        }

        /// Add a writable descriptor of the given size.
        fn add_writable(&mut self, mem_acc: &MemAccessor, len: u32) -> u16 {
            let gpa = self.alloc_data(len);
            self.write_desc(mem_acc, gpa, len, DescFlag::WRITE.bits(), 0)
        }

        /// Publish a descriptor chain head on the available ring.
        fn publish_avail(&mut self, mem_acc: &MemAccessor, head: u16) {
            let slot = self.layout.avail_base
                + 4
                + u64::from(self.avail_idx % QUEUE_SIZE) * 2;
            self.avail_idx += 1;
            let new_idx = self.avail_idx;
            let mem = mem_acc.access().unwrap();
            mem.write(GuestAddr(slot), &head);
            mem.write(GuestAddr(self.layout.avail_base + 2), &new_idx);
        }

        /// Chain two descriptors together via NEXT flag.
        fn chain(&self, mem_acc: &MemAccessor, from: u16, to: u16) {
            let gpa = self.layout.desc_base + u64::from(from) * 16;
            let mem = mem_acc.access().unwrap();
            let mut raw: RawDesc = *mem.read(GuestAddr(gpa)).unwrap();
            raw.flags |= DescFlag::NEXT.bits();
            raw.next = to;
            mem.write(GuestAddr(gpa), &raw);
        }

        /// Read the used ring index.
        fn used_idx(&self, mem_acc: &MemAccessor) -> u16 {
            let mem = mem_acc.access().unwrap();
            *mem.read(GuestAddr(self.layout.used_base + 2)).unwrap()
        }

        /// Read the header and data payload from a used ring entry by index.
        fn read_used_entry(
            &self,
            mem_acc: &MemAccessor,
            used_index: u16,
        ) -> (VsockPacketHeader, Vec<u8>) {
            let mem = mem_acc.access().unwrap();
            let entry_gpa = self.layout.used_base
                + 4
                + u64::from(used_index % QUEUE_SIZE) * 8;
            let desc_id: u32 = *mem.read(GuestAddr(entry_gpa)).unwrap();

            let desc_gpa =
                self.layout.desc_base + u64::from(desc_id as u16) * 16;
            let raw_desc: RawDesc = *mem.read(GuestAddr(desc_gpa)).unwrap();

            let hdr_size = std::mem::size_of::<VsockPacketHeader>();
            let mut hdr = VsockPacketHeader::default();
            mem.read_into(
                GuestAddr(raw_desc.addr),
                &mut crate::common::GuestData::from(unsafe {
                    std::slice::from_raw_parts_mut(
                        &mut hdr as *mut VsockPacketHeader as *mut u8,
                        hdr_size,
                    )
                }),
                hdr_size,
            );

            let data_len = hdr.len() as usize;
            let mut data = vec![0u8; data_len];
            if data_len > 0 {
                mem.read_into(
                    GuestAddr(raw_desc.addr + hdr_size as u64),
                    &mut crate::common::GuestData::from(data.as_mut_slice()),
                    data_len,
                );
            }

            (hdr, data)
        }
    }

    #[test]
    fn request_receives_response() {
        let vsock_port = 3000;
        let guest_port = 1234;
        let guest_cid: u32 = 50;
        let (_listener, backends) = bind_test_backend(vsock_port);

        let (vq, tv) = make_test_vsock_vq();
        let log = test_logger();
        let poller = VsockPoller::new(guest_cid, vq, log, backends).unwrap();

        let rx_data_start = tv.event.end + PAGE_SIZE;
        let mut rx_writer = QueueWriter::new(tv.rx, rx_data_start);
        let _d_rx = rx_writer.add_writable(&tv.mem_acc, 256);
        rx_writer.publish_avail(&tv.mem_acc, _d_rx);

        let notify = poller.notify_handle();
        let handle = poller.run();

        let tx_data_start = rx_data_start + PAGE_SIZE;
        let mut tx_writer = QueueWriter::new(tv.tx, tx_data_start);

        let mut hdr = VsockPacketHeader::default();
        hdr.set_src_cid(guest_cid)
            .set_dst_cid(VSOCK_HOST_CID as u32)
            .set_src_port(guest_port)
            .set_dst_port(vsock_port)
            .set_len(0)
            .set_socket_type(VIRTIO_VSOCK_TYPE_STREAM)
            .set_op(VIRTIO_VSOCK_OP_REQUEST)
            .set_buf_alloc(65536)
            .set_fwd_cnt(0);

        let d_tx = tx_writer.add_readable(&tv.mem_acc, hdr_as_bytes(&hdr));
        tx_writer.publish_avail(&tv.mem_acc, d_tx);
        notify.queue_notify(crate::hw::virtio::vsock::VSOCK_TX_QUEUE).unwrap();

        wait_for_used(&rx_writer, &tv.mem_acc, 1, 5000);

        let (resp_hdr, _) = rx_writer.read_used_entry(&tv.mem_acc, 0);
        assert_eq!(resp_hdr.op(), VIRTIO_VSOCK_OP_RESPONSE);
        assert_eq!(resp_hdr.src_cid(), VSOCK_HOST_CID);
        assert_eq!(resp_hdr.dst_cid(), guest_cid as u64);
        assert_eq!(resp_hdr.src_port(), vsock_port);
        assert_eq!(resp_hdr.dst_port(), guest_port);
        assert_eq!(resp_hdr.socket_type(), VIRTIO_VSOCK_TYPE_STREAM);

        notify.shutdown().unwrap();
        handle.join().unwrap();
    }

    #[test]
    fn rw_with_invalid_socket_type_receives_rst() {
        let guest_cid: u32 = 50;
        let (vq, tv) = make_test_vsock_vq();

        let log = test_logger();
        let poller =
            VsockPoller::new(guest_cid, vq, log, IdHashMap::new()).unwrap();

        let rx_data_start = tv.event.end + PAGE_SIZE;
        let mut rx_writer = QueueWriter::new(tv.rx, rx_data_start);
        let _d_rx = rx_writer.add_writable(&tv.mem_acc, 256);
        rx_writer.publish_avail(&tv.mem_acc, _d_rx);

        let notify = poller.notify_handle();
        let handle = poller.run();

        let tx_data_start = rx_data_start + PAGE_SIZE;
        let mut tx_writer = QueueWriter::new(tv.tx, tx_data_start);

        let invalid_socket_type: u16 = 0xBEEF;
        let mut hdr = VsockPacketHeader::default();
        hdr.set_src_cid(guest_cid)
            .set_dst_cid(VSOCK_HOST_CID as u32)
            .set_src_port(5555)
            .set_dst_port(8080)
            .set_len(0)
            .set_socket_type(invalid_socket_type)
            .set_op(VIRTIO_VSOCK_OP_RW)
            .set_buf_alloc(65536)
            .set_fwd_cnt(0);

        let d_tx = tx_writer.add_readable(&tv.mem_acc, hdr_as_bytes(&hdr));
        tx_writer.publish_avail(&tv.mem_acc, d_tx);
        notify.queue_notify(crate::hw::virtio::vsock::VSOCK_TX_QUEUE).unwrap();

        wait_for_used(&rx_writer, &tv.mem_acc, 1, 5000);

        let (resp_hdr, _) = rx_writer.read_used_entry(&tv.mem_acc, 0);
        assert_eq!(resp_hdr.op(), VIRTIO_VSOCK_OP_RST);
        assert_eq!(resp_hdr.src_cid(), VSOCK_HOST_CID);
        assert_eq!(resp_hdr.dst_cid(), guest_cid as u64);
        assert_eq!(resp_hdr.src_port(), 8080);
        assert_eq!(resp_hdr.dst_port(), 5555);

        notify.shutdown().unwrap();
        handle.join().unwrap();
    }

    #[test]
    fn request_then_rw_delivers_data() {
        let vsock_port = 3000;
        let guest_port = 1234;
        let guest_cid: u32 = 50;
        let (listener, backends) = bind_test_backend(vsock_port);
        listener.set_nonblocking(false).unwrap();

        let (vq, tv) = make_test_vsock_vq();
        let log = test_logger();
        let poller = VsockPoller::new(guest_cid, vq, log, backends).unwrap();

        let rx_data_start = tv.event.end + PAGE_SIZE;
        let mut rx_writer = QueueWriter::new(tv.rx, rx_data_start);
        for _ in 0..4 {
            let d = rx_writer.add_writable(&tv.mem_acc, 4096);
            rx_writer.publish_avail(&tv.mem_acc, d);
        }

        let notify = poller.notify_handle();
        let handle = poller.run();

        let tx_data_start = rx_data_start + PAGE_SIZE * 8;
        let mut tx_writer = QueueWriter::new(tv.tx, tx_data_start);

        // Send REQUEST
        let mut req_hdr = VsockPacketHeader::default();
        req_hdr
            .set_src_cid(guest_cid)
            .set_dst_cid(VSOCK_HOST_CID as u32)
            .set_src_port(guest_port)
            .set_dst_port(vsock_port)
            .set_len(0)
            .set_socket_type(VIRTIO_VSOCK_TYPE_STREAM)
            .set_op(VIRTIO_VSOCK_OP_REQUEST)
            .set_buf_alloc(65536)
            .set_fwd_cnt(0);

        let d_tx = tx_writer.add_readable(&tv.mem_acc, hdr_as_bytes(&req_hdr));
        tx_writer.publish_avail(&tv.mem_acc, d_tx);
        notify.queue_notify(crate::hw::virtio::vsock::VSOCK_TX_QUEUE).unwrap();

        // Accept TCP connection and wait for RESPONSE
        let mut accepted = listener.accept().unwrap().0;
        accepted.set_nonblocking(false).unwrap();
        accepted.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        wait_for_used(&rx_writer, &tv.mem_acc, 1, 5000);

        // Send RW packet with data payload
        let payload = b"hello from guest via vsock!";
        let mut rw_hdr = VsockPacketHeader::default();
        rw_hdr
            .set_src_cid(guest_cid)
            .set_dst_cid(VSOCK_HOST_CID as u32)
            .set_src_port(guest_port)
            .set_dst_port(vsock_port)
            .set_len(payload.len() as u32)
            .set_socket_type(VIRTIO_VSOCK_TYPE_STREAM)
            .set_op(VIRTIO_VSOCK_OP_RW)
            .set_buf_alloc(65536)
            .set_fwd_cnt(0);

        let d_hdr = tx_writer.add_readable(&tv.mem_acc, hdr_as_bytes(&rw_hdr));
        let d_body = tx_writer.add_readable(&tv.mem_acc, payload);
        tx_writer.chain(&tv.mem_acc, d_hdr, d_body);
        tx_writer.publish_avail(&tv.mem_acc, d_hdr);
        notify.queue_notify(crate::hw::virtio::vsock::VSOCK_TX_QUEUE).unwrap();

        // Read from accepted TCP stream and verify
        let mut buf = vec![0u8; payload.len()];
        accepted.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, payload);

        notify.shutdown().unwrap();
        handle.join().unwrap();
    }

    /// Helper: serialize a VsockPacketHeader to bytes.
    fn hdr_as_bytes(hdr: &VsockPacketHeader) -> &[u8] {
        unsafe {
            std::slice::from_raw_parts(
                hdr as *const VsockPacketHeader as *const u8,
                std::mem::size_of::<VsockPacketHeader>(),
            )
        }
    }

    #[test]
    fn credit_update_sent_after_flushing_half_buffer() {
        let vsock_port = 4000;
        let guest_port = 2000;
        let guest_cid: u32 = 50;
        let (listener, backends) = bind_test_backend(vsock_port);
        listener.set_nonblocking(false).unwrap();

        let (vq, tv) = make_test_vsock_vq();
        let log = test_logger();
        let poller = VsockPoller::new(guest_cid, vq, log, backends).unwrap();

        // Provide plenty of RX descriptors for RESPONSE + credit updates
        let rx_data_start = tv.event.end + PAGE_SIZE;
        let mut rx_writer = QueueWriter::new(tv.rx, rx_data_start);
        for _ in 0..16 {
            let d = rx_writer.add_writable(&tv.mem_acc, 4096);
            rx_writer.publish_avail(&tv.mem_acc, d);
        }

        let notify = poller.notify_handle();
        let handle = poller.run();

        // Establish connection
        let tx_data_start = rx_data_start + PAGE_SIZE * 32;
        let tx_data_base = tx_data_start;
        let mut tx_writer = QueueWriter::new(tv.tx, tx_data_start);

        let mut req_hdr = VsockPacketHeader::default();
        req_hdr
            .set_src_cid(guest_cid)
            .set_dst_cid(VSOCK_HOST_CID as u32)
            .set_src_port(guest_port)
            .set_dst_port(vsock_port)
            .set_len(0)
            .set_socket_type(VIRTIO_VSOCK_TYPE_STREAM)
            .set_op(VIRTIO_VSOCK_OP_REQUEST)
            .set_buf_alloc(65536)
            .set_fwd_cnt(0);

        let d_tx = tx_writer.add_readable(&tv.mem_acc, hdr_as_bytes(&req_hdr));
        tx_writer.publish_avail(&tv.mem_acc, d_tx);
        notify.queue_notify(crate::hw::virtio::vsock::VSOCK_TX_QUEUE).unwrap();

        let mut accepted = listener.accept().unwrap().0;
        accepted.set_nonblocking(false).unwrap();
        accepted.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        wait_for_used(&rx_writer, &tv.mem_acc, 1, 5000);

        // Send enough data to exceed half the buffer capacity (64KB).
        let chunk_size = 8192;
        let num_chunks = (CONN_TX_BUF_SIZE / 2) / chunk_size + 1;
        let payload = vec![0xAB_u8; chunk_size];
        let total_sent = num_chunks * chunk_size;
        let mut tx_consumed = 1u16; // REQUEST was consumed

        for _ in 0..num_chunks {
            // Reuse descriptor slots each iteration
            tx_writer.next_desc = 0;
            tx_writer.data_cursor = tx_data_base;

            let mut rw_hdr = VsockPacketHeader::default();
            rw_hdr
                .set_src_cid(guest_cid)
                .set_dst_cid(VSOCK_HOST_CID as u32)
                .set_src_port(guest_port)
                .set_dst_port(vsock_port)
                .set_len(payload.len() as u32)
                .set_socket_type(VIRTIO_VSOCK_TYPE_STREAM)
                .set_op(VIRTIO_VSOCK_OP_RW)
                .set_buf_alloc(65536)
                .set_fwd_cnt(0);

            let d_hdr =
                tx_writer.add_readable(&tv.mem_acc, hdr_as_bytes(&rw_hdr));
            let d_body = tx_writer.add_readable(&tv.mem_acc, &payload);
            tx_writer.chain(&tv.mem_acc, d_hdr, d_body);
            tx_writer.publish_avail(&tv.mem_acc, d_hdr);
            notify
                .queue_notify(crate::hw::virtio::vsock::VSOCK_TX_QUEUE)
                .unwrap();

            tx_consumed += 1;
            wait_for_used(&tx_writer, &tv.mem_acc, tx_consumed, 5000);
        }

        // Drain the data from the accepted socket to confirm it arrived
        let mut buf = vec![0u8; total_sent];
        accepted.read_exact(&mut buf).unwrap();
        assert!(buf.iter().all(|&b| b == 0xAB));

        // Look for a CREDIT_UPDATE in the RX used entries
        let rx_used = rx_writer.used_idx(&tv.mem_acc);
        assert!(rx_used >= 2, "expected at least RESPONSE + CREDIT_UPDATE");

        let mut found_credit_update = false;
        for i in 1..rx_used {
            let (hdr, _) = rx_writer.read_used_entry(&tv.mem_acc, i);
            if hdr.op() == VIRTIO_VSOCK_OP_CREDIT_UPDATE {
                assert_eq!(hdr.src_cid(), VSOCK_HOST_CID);
                assert_eq!(hdr.dst_cid(), guest_cid as u64);
                assert_eq!(hdr.src_port(), vsock_port);
                assert_eq!(hdr.dst_port(), guest_port);
                assert_eq!(hdr.buf_alloc(), CONN_TX_BUF_SIZE as u32);
                found_credit_update = true;
                break;
            }
        }
        assert!(found_credit_update, "expected a CREDIT_UPDATE on RX queue");

        notify.shutdown().unwrap();
        handle.join().unwrap();
    }

    #[test]
    fn rst_removes_established_connection() {
        let vsock_port = 5000;
        let guest_port = 3000;
        let guest_cid: u32 = 50;
        let (listener, backends) = bind_test_backend(vsock_port);
        listener.set_nonblocking(false).unwrap();

        let (vq, tv) = make_test_vsock_vq();
        let log = test_logger();
        let poller = VsockPoller::new(guest_cid, vq, log, backends).unwrap();

        let rx_data_start = tv.event.end + PAGE_SIZE;
        let mut rx_writer = QueueWriter::new(tv.rx, rx_data_start);
        for _ in 0..4 {
            let d = rx_writer.add_writable(&tv.mem_acc, 4096);
            rx_writer.publish_avail(&tv.mem_acc, d);
        }

        let notify = poller.notify_handle();
        let handle = poller.run();

        let tx_data_start = rx_data_start + PAGE_SIZE * 8;
        let mut tx_writer = QueueWriter::new(tv.tx, tx_data_start);

        // Send REQUEST
        let mut req_hdr = VsockPacketHeader::default();
        req_hdr
            .set_src_cid(guest_cid)
            .set_dst_cid(VSOCK_HOST_CID as u32)
            .set_src_port(guest_port)
            .set_dst_port(vsock_port)
            .set_len(0)
            .set_socket_type(VIRTIO_VSOCK_TYPE_STREAM)
            .set_op(VIRTIO_VSOCK_OP_REQUEST)
            .set_buf_alloc(65536)
            .set_fwd_cnt(0);

        let d_tx = tx_writer.add_readable(&tv.mem_acc, hdr_as_bytes(&req_hdr));
        tx_writer.publish_avail(&tv.mem_acc, d_tx);
        notify.queue_notify(crate::hw::virtio::vsock::VSOCK_TX_QUEUE).unwrap();

        let mut accepted = listener.accept().unwrap().0;
        accepted.set_nonblocking(false).unwrap();
        accepted.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        wait_for_used(&rx_writer, &tv.mem_acc, 1, 5000);

        // Send RST
        let mut rst_hdr = VsockPacketHeader::default();
        rst_hdr
            .set_src_cid(guest_cid)
            .set_dst_cid(VSOCK_HOST_CID as u32)
            .set_src_port(guest_port)
            .set_dst_port(vsock_port)
            .set_len(0)
            .set_socket_type(VIRTIO_VSOCK_TYPE_STREAM)
            .set_op(VIRTIO_VSOCK_OP_RST)
            .set_buf_alloc(0)
            .set_fwd_cnt(0);

        let d_rst = tx_writer.add_readable(&tv.mem_acc, hdr_as_bytes(&rst_hdr));
        tx_writer.publish_avail(&tv.mem_acc, d_rst);
        notify.queue_notify(crate::hw::virtio::vsock::VSOCK_TX_QUEUE).unwrap();

        // Wait for the RST to be consumed
        wait_for_used(&tx_writer, &tv.mem_acc, 2, 5000);

        // Verify the TCP connection was closed by reading from the
        // accepted stream — should get EOF or error.
        let mut buf = [0u8; 1];
        let result = accepted.read(&mut buf);
        match result {
            Ok(0) => {}  // EOF — connection closed
            Err(_) => {} // Error — also acceptable
            Ok(n) => panic!("expected EOF or error, got {n} bytes"),
        }

        notify.shutdown().unwrap();
        handle.join().unwrap();
    }

    /// Spin on `used_idx` until it reaches `expected`, with a timeout.
    fn wait_for_used(
        writer: &QueueWriter,
        mem_acc: &MemAccessor,
        expected: u16,
        timeout_ms: u64,
    ) {
        let start = std::time::Instant::now();
        let timeout = Duration::from_millis(timeout_ms);
        loop {
            if writer.used_idx(mem_acc) >= expected {
                return;
            }
            if start.elapsed() > timeout {
                panic!(
                    "timed out waiting for used_idx to reach {} (current: {})",
                    expected,
                    writer.used_idx(mem_acc),
                );
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn end_to_end_guest_to_host() {
        let vsock_port = 7000;
        let guest_port = 5000;
        let guest_cid: u32 = 50;
        let (listener, backends) = bind_test_backend(vsock_port);
        listener.set_nonblocking(false).unwrap();

        let (vq, tv) = make_test_vsock_vq();
        let log = test_logger();
        let poller = VsockPoller::new(guest_cid, vq, log, backends).unwrap();

        // Pre-populate RX queue with writable descriptors for RESPONSE + data
        let rx_data_start = tv.event.end + PAGE_SIZE;
        let mut rx_writer = QueueWriter::new(tv.rx, rx_data_start);
        for _ in 0..8 {
            let d = rx_writer.add_writable(&tv.mem_acc, 4096);
            rx_writer.publish_avail(&tv.mem_acc, d);
        }

        let notify = poller.notify_handle();
        let handle = poller.run();

        // -- Write REQUEST packet into TX queue --
        let tx_data_start = rx_data_start + PAGE_SIZE * 16;
        let mut tx_writer = QueueWriter::new(tv.tx, tx_data_start);

        let mut req_hdr = VsockPacketHeader::default();
        req_hdr
            .set_src_cid(guest_cid)
            .set_dst_cid(VSOCK_HOST_CID as u32)
            .set_src_port(guest_port)
            .set_dst_port(vsock_port)
            .set_len(0)
            .set_socket_type(VIRTIO_VSOCK_TYPE_STREAM)
            .set_op(VIRTIO_VSOCK_OP_REQUEST)
            .set_buf_alloc(65536)
            .set_fwd_cnt(0);

        let d_tx = tx_writer.add_readable(&tv.mem_acc, hdr_as_bytes(&req_hdr));
        tx_writer.publish_avail(&tv.mem_acc, d_tx);
        notify.queue_notify(crate::hw::virtio::vsock::VSOCK_TX_QUEUE).unwrap();

        // Accept the TCP connection (blocks until poller connects)
        let mut accepted = listener.accept().unwrap().0;
        accepted.set_nonblocking(false).unwrap();
        accepted.set_read_timeout(Some(Duration::from_secs(5))).unwrap();

        // Wait for RESPONSE on RX queue
        wait_for_used(&rx_writer, &tv.mem_acc, 1, 5000);

        // -- Guest→Host: send RW packet with payload --
        let payload = b"hello from guest via vsock end-to-end!";
        let mut rw_hdr = VsockPacketHeader::default();
        rw_hdr
            .set_src_cid(guest_cid)
            .set_dst_cid(VSOCK_HOST_CID as u32)
            .set_src_port(guest_port)
            .set_dst_port(vsock_port)
            .set_len(payload.len() as u32)
            .set_socket_type(VIRTIO_VSOCK_TYPE_STREAM)
            .set_op(VIRTIO_VSOCK_OP_RW)
            .set_buf_alloc(65536)
            .set_fwd_cnt(0);

        let d_hdr = tx_writer.add_readable(&tv.mem_acc, hdr_as_bytes(&rw_hdr));
        let d_body = tx_writer.add_readable(&tv.mem_acc, payload);
        tx_writer.chain(&tv.mem_acc, d_hdr, d_body);
        tx_writer.publish_avail(&tv.mem_acc, d_hdr);
        notify.queue_notify(crate::hw::virtio::vsock::VSOCK_TX_QUEUE).unwrap();

        // Read from accepted TCP stream — verify guest→host data
        let mut buf = vec![0u8; payload.len()];
        accepted.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, payload, "guest→host data mismatch");

        // -- Host→Guest: write data into accepted TCP stream --
        let host_payload = b"reply from host via vsock!";
        accepted.write_all(host_payload).unwrap();
        accepted.flush().unwrap();

        // Wait for RW packet on RX queue (RESPONSE was 1, now expect 2+)
        wait_for_used(&rx_writer, &tv.mem_acc, 2, 5000);

        // Read back the RW packet from RX used ring entry 1
        let (resp_hdr, host_buf) = rx_writer.read_used_entry(&tv.mem_acc, 1);

        assert_eq!(resp_hdr.op(), VIRTIO_VSOCK_OP_RW);
        assert_eq!(resp_hdr.src_port(), vsock_port);
        assert_eq!(resp_hdr.dst_port(), guest_port);
        assert_eq!(&host_buf, host_payload, "host→guest data mismatch");

        // Shutdown and join
        notify.shutdown().unwrap();
        handle.join().unwrap();
    }

    #[test]
    fn rx_blocked_resumes_when_descriptors_available() {
        let vsock_port = 6000;
        let guest_port = 4000;
        let guest_cid: u32 = 50;
        let (listener, backends) = bind_test_backend(vsock_port);
        listener.set_nonblocking(false).unwrap();

        let (vq, tv) = make_test_vsock_vq();
        let log = test_logger();
        let poller = VsockPoller::new(guest_cid, vq, log, backends).unwrap();

        // Provide only ONE RX descriptor — just enough for the RESPONSE.
        let rx_data_start = tv.event.end + PAGE_SIZE;
        let rx_data_base = rx_data_start;
        let mut rx_writer = QueueWriter::new(tv.rx, rx_data_start);
        let _d_rx = rx_writer.add_writable(&tv.mem_acc, 4096);
        rx_writer.publish_avail(&tv.mem_acc, _d_rx);

        let notify = poller.notify_handle();
        let handle = poller.run();

        let tx_data_start = rx_data_start + PAGE_SIZE * 16;
        let mut tx_writer = QueueWriter::new(tv.tx, tx_data_start);

        // Send REQUEST
        let mut req_hdr = VsockPacketHeader::default();
        req_hdr
            .set_src_cid(guest_cid)
            .set_dst_cid(VSOCK_HOST_CID as u32)
            .set_src_port(guest_port)
            .set_dst_port(vsock_port)
            .set_len(0)
            .set_socket_type(VIRTIO_VSOCK_TYPE_STREAM)
            .set_op(VIRTIO_VSOCK_OP_REQUEST)
            .set_buf_alloc(65536)
            .set_fwd_cnt(0);

        let d_tx = tx_writer.add_readable(&tv.mem_acc, hdr_as_bytes(&req_hdr));
        tx_writer.publish_avail(&tv.mem_acc, d_tx);
        notify.queue_notify(crate::hw::virtio::vsock::VSOCK_TX_QUEUE).unwrap();

        let mut accepted = listener.accept().unwrap().0;
        accepted.set_nonblocking(false).unwrap();
        wait_for_used(&rx_writer, &tv.mem_acc, 1, 5000);

        // The RESPONSE consumed the only RX descriptor. Write data from
        // the host side — delivery should be blocked until we add more
        // RX descriptors.
        let host_data = b"data from the host side";
        accepted.write_all(host_data).unwrap();
        accepted.flush().unwrap();

        // Give the poller time to attempt delivery (and get blocked)
        std::thread::sleep(Duration::from_millis(100));

        // Verify no new used entries appeared (still just the RESPONSE)
        assert_eq!(rx_writer.used_idx(&tv.mem_acc), 1);

        // Add new RX descriptors and notify
        rx_writer.next_desc = 0;
        rx_writer.data_cursor = rx_data_base;
        let d_rx2 = rx_writer.add_writable(&tv.mem_acc, 4096);
        rx_writer.publish_avail(&tv.mem_acc, d_rx2);
        notify.queue_notify(crate::hw::virtio::vsock::VSOCK_RX_QUEUE).unwrap();

        // Wait for the data to be delivered
        wait_for_used(&rx_writer, &tv.mem_acc, 2, 5000);

        let (rw_hdr, payload) = rx_writer.read_used_entry(&tv.mem_acc, 1);
        assert_eq!(rw_hdr.op(), VIRTIO_VSOCK_OP_RW);
        assert_eq!(rw_hdr.src_port(), vsock_port);
        assert_eq!(rw_hdr.dst_port(), guest_port);
        assert_eq!(&payload, host_data);

        notify.shutdown().unwrap();
        handle.join().unwrap();
    }

    /// End-to-end test with large data transfers in both directions,
    /// exercising rx_blocked, credit updates, and descriptor replenishment
    /// across many batches of reused descriptor slots.
    #[test]
    fn end_to_end_large_data() {
        let total_bytes: usize = 10 * 1024 * 1024;

        let vsock_port = 8000;
        let guest_port = 6000;
        let guest_cid: u32 = 50;
        let (listener, backends) = bind_test_backend(vsock_port);
        listener.set_nonblocking(false).unwrap();

        let (vq, tv) = make_test_vsock_vq();
        let log = test_logger();
        let poller = VsockPoller::new(guest_cid, vq, log, backends).unwrap();

        // Provide initial RX descriptors for RESPONSE + credit updates
        let rx_data_start = tv.event.end + PAGE_SIZE;
        let rx_data_base = rx_data_start;
        let mut rx_writer = QueueWriter::new(tv.rx, rx_data_start);
        for _ in 0..8 {
            let d = rx_writer.add_writable(&tv.mem_acc, 4096);
            rx_writer.publish_avail(&tv.mem_acc, d);
        }

        let notify = poller.notify_handle();
        let handle = poller.run();

        // -- Establish connection --
        let tx_data_start = rx_data_start + PAGE_SIZE * 32;
        let tx_data_base = tx_data_start;
        let mut tx_writer = QueueWriter::new(tv.tx, tx_data_start);

        // Use a large buf_alloc so host→guest credit doesn't run out
        // before we've transferred all the data.
        let buf_alloc = total_bytes as u32 * 2;

        let mut req_hdr = VsockPacketHeader::default();
        req_hdr
            .set_src_cid(guest_cid)
            .set_dst_cid(VSOCK_HOST_CID as u32)
            .set_src_port(guest_port)
            .set_dst_port(vsock_port)
            .set_len(0)
            .set_socket_type(VIRTIO_VSOCK_TYPE_STREAM)
            .set_op(VIRTIO_VSOCK_OP_REQUEST)
            .set_buf_alloc(buf_alloc)
            .set_fwd_cnt(0);

        let d_tx = tx_writer.add_readable(&tv.mem_acc, hdr_as_bytes(&req_hdr));
        tx_writer.publish_avail(&tv.mem_acc, d_tx);
        notify.queue_notify(crate::hw::virtio::vsock::VSOCK_TX_QUEUE).unwrap();

        let accepted = listener.accept().unwrap().0;
        accepted.set_nonblocking(false).unwrap();

        wait_for_used(&rx_writer, &tv.mem_acc, 1, 5000);

        // ============================================================
        // Phase 1: Guest→Host
        //
        // A reader thread drains the TCP socket while the main thread
        // injects RW packets in batches, reusing descriptor slots and
        // guest memory between batches.
        // ============================================================
        let guest_data: Vec<u8> =
            (0..total_bytes).map(|i| (i % 251) as u8).collect();

        // Track how many bytes the reader has consumed so we can apply
        // backpressure and avoid overflowing the poller's VsockBuf.
        let bytes_read = Arc::new(AtomicUsize::new(0));
        let tcp_reader = {
            let mut stream = accepted.try_clone().unwrap();
            let len = total_bytes;
            let progress = Arc::clone(&bytes_read);
            std::thread::spawn(move || {
                let mut result = Vec::with_capacity(len);
                let mut chunk = vec![0u8; 65536];
                let mut total = 0;
                while total < len {
                    let n = stream.read(&mut chunk).unwrap();
                    assert!(n > 0, "unexpected EOF after {total}/{len}");
                    result.extend_from_slice(&chunk[..n]);
                    total += n;
                    progress.store(total, Ordering::Release);
                }
                result
            })
        };

        let chunk_size = 4096;
        let batch_packets = 8; // 8 packets × 2 descs = 16 descs per batch
        let mut guest_sent = 0usize;
        // TX used_idx starts at 1 (the REQUEST was consumed)
        let mut tx_consumed = 1u16;

        while guest_sent < total_bytes {
            let remaining = (total_bytes - guest_sent).div_ceil(chunk_size);
            let this_batch = std::cmp::min(batch_packets, remaining);
            // Backpressure: don't let in-flight data exceed VsockBuf
            // capacity. The poller buffers TX data in VsockBuf (128KB)
            // and flushes via POLLOUT. If we push faster than the
            // flush rate, the buffer overflows and panics.
            let after_send = guest_sent + this_batch * chunk_size;
            loop {
                let read = bytes_read.load(Ordering::Acquire);
                if after_send <= read + CONN_TX_BUF_SIZE {
                    break;
                }
                std::thread::sleep(Duration::from_millis(1));
            }

            // Reuse the same descriptor slots and data region each batch.
            // Safe because we wait for the previous batch to be fully
            // consumed before overwriting.
            tx_writer.next_desc = 0;
            tx_writer.data_cursor = tx_data_base;

            for i in 0..this_batch {
                let offset = guest_sent + i * chunk_size;
                let end = std::cmp::min(offset + chunk_size, total_bytes);
                let payload = &guest_data[offset..end];

                let mut rw_hdr = VsockPacketHeader::default();
                rw_hdr
                    .set_src_cid(guest_cid)
                    .set_dst_cid(VSOCK_HOST_CID as u32)
                    .set_src_port(guest_port)
                    .set_dst_port(vsock_port)
                    .set_len(payload.len() as u32)
                    .set_socket_type(VIRTIO_VSOCK_TYPE_STREAM)
                    .set_op(VIRTIO_VSOCK_OP_RW)
                    .set_buf_alloc(buf_alloc)
                    .set_fwd_cnt(0);

                let d_hdr =
                    tx_writer.add_readable(&tv.mem_acc, hdr_as_bytes(&rw_hdr));
                let d_body = tx_writer.add_readable(&tv.mem_acc, payload);
                tx_writer.chain(&tv.mem_acc, d_hdr, d_body);
                tx_writer.publish_avail(&tv.mem_acc, d_hdr);
            }

            notify
                .queue_notify(crate::hw::virtio::vsock::VSOCK_TX_QUEUE)
                .unwrap();

            // Wait for the poller to consume this entire batch before
            // we overwrite the descriptor slots in the next iteration.
            tx_consumed += this_batch as u16;
            wait_for_used(&tx_writer, &tv.mem_acc, tx_consumed, 10000);

            guest_sent += this_batch * chunk_size;
            if guest_sent > total_bytes {
                guest_sent = total_bytes;
            }
        }

        let received = tcp_reader.join().unwrap();
        assert_eq!(received.len(), total_bytes);
        assert!(received == guest_data, "guest→host data mismatch");

        // ============================================================
        // Phase 2: Host→Guest
        //
        // A writer thread pushes data into the TCP socket while the
        // main thread replenishes RX descriptors in batches, reads
        // completed used entries, and reuses descriptor slots once
        // the entire batch has been consumed.
        // ============================================================
        let host_data: Vec<u8> =
            (0..total_bytes).map(|i| ((i + 7) % 251) as u8).collect();

        let tcp_writer = {
            let mut stream = accepted.try_clone().unwrap();
            let data = host_data.clone();
            std::thread::spawn(move || {
                stream.write_all(&data).unwrap();
            })
        };

        let mut host_to_guest = Vec::with_capacity(total_bytes);

        // Skip all used entries produced before this phase (RESPONSE +
        // any credit updates from Phase 1).
        let mut rx_next_used = rx_writer.used_idx(&tv.mem_acc);
        let rx_batch = 16u16;
        let mut descs_outstanding = 0u16;

        while host_to_guest.len() < total_bytes {
            // When all outstanding descriptors have been consumed we can
            // safely reuse the descriptor slots and data region.
            if descs_outstanding == 0 {
                rx_writer.next_desc = 0;
                rx_writer.data_cursor = rx_data_base;

                for _ in 0..rx_batch {
                    let d = rx_writer.add_writable(&tv.mem_acc, 4096);
                    rx_writer.publish_avail(&tv.mem_acc, d);
                    descs_outstanding += 1;
                }
                notify
                    .queue_notify(crate::hw::virtio::vsock::VSOCK_RX_QUEUE)
                    .unwrap();
            }

            // Wait for at least one new used entry.
            wait_for_used(&rx_writer, &tv.mem_acc, rx_next_used + 1, 10000);

            // Drain all currently available used entries.
            let current_used = rx_writer.used_idx(&tv.mem_acc);
            while rx_next_used < current_used {
                let (hdr, data) =
                    rx_writer.read_used_entry(&tv.mem_acc, rx_next_used);
                rx_next_used += 1;
                descs_outstanding -= 1;

                if hdr.op() == VIRTIO_VSOCK_OP_RW {
                    host_to_guest.extend_from_slice(&data);
                }
                // Credit updates and other control packets are
                // silently consumed — they're expected here.
            }
        }

        tcp_writer.join().unwrap();
        assert_eq!(host_to_guest.len(), total_bytes);
        assert!(host_to_guest == host_data, "host→guest data mismatch");

        // Shutdown and join
        notify.shutdown().unwrap();
        handle.join().unwrap();
    }

    /// Closing the host-side TCP socket should cause the poller to send
    /// a VIRTIO_VSOCK_OP_SHUTDOWN packet with VIRTIO_VSOCK_SHUTDOWN_F_SEND
    /// to the guest, indicating the host will no longer send data.
    #[test]
    fn host_socket_eof_sends_shutdown() {
        let vsock_port = 9000;
        let guest_port = 7000;
        let guest_cid: u32 = 50;
        let (listener, backends) = bind_test_backend(vsock_port);
        listener.set_nonblocking(false).unwrap();

        let (vq, tv) = make_test_vsock_vq();
        let log = test_logger();
        let poller = VsockPoller::new(guest_cid, vq, log, backends).unwrap();

        // Provide RX descriptors for RESPONSE + SHUTDOWN
        let rx_data_start = tv.event.end + PAGE_SIZE;
        let mut rx_writer = QueueWriter::new(tv.rx, rx_data_start);
        for _ in 0..4 {
            let d = rx_writer.add_writable(&tv.mem_acc, 4096);
            rx_writer.publish_avail(&tv.mem_acc, d);
        }

        let notify = poller.notify_handle();
        let handle = poller.run();

        // -- Establish connection --
        let tx_data_start = rx_data_start + PAGE_SIZE * 16;
        let mut tx_writer = QueueWriter::new(tv.tx, tx_data_start);

        let mut req_hdr = VsockPacketHeader::default();
        req_hdr
            .set_src_cid(guest_cid)
            .set_dst_cid(VSOCK_HOST_CID as u32)
            .set_src_port(guest_port)
            .set_dst_port(vsock_port)
            .set_len(0)
            .set_socket_type(VIRTIO_VSOCK_TYPE_STREAM)
            .set_op(VIRTIO_VSOCK_OP_REQUEST)
            .set_buf_alloc(65536)
            .set_fwd_cnt(0);

        let d_tx = tx_writer.add_readable(&tv.mem_acc, hdr_as_bytes(&req_hdr));
        tx_writer.publish_avail(&tv.mem_acc, d_tx);
        notify.queue_notify(crate::hw::virtio::vsock::VSOCK_TX_QUEUE).unwrap();

        // Accept the connection, wait for RESPONSE
        let accepted = listener.accept().unwrap().0;
        wait_for_used(&rx_writer, &tv.mem_acc, 1, 5000);

        // -- Close the host-side socket to produce EOF --
        drop(accepted);

        // The poller should detect EOF on the next POLLIN and send
        // a SHUTDOWN packet to the guest.
        wait_for_used(&rx_writer, &tv.mem_acc, 2, 5000);

        // Read back the packet from RX used ring entry 1
        let (hdr, _data) = rx_writer.read_used_entry(&tv.mem_acc, 1);

        assert_eq!(hdr.op(), VIRTIO_VSOCK_OP_SHUTDOWN);
        assert_eq!(hdr.src_cid(), VSOCK_HOST_CID);
        assert_eq!(hdr.dst_cid(), guest_cid as u64);
        assert_eq!(hdr.src_port(), vsock_port);
        assert_eq!(hdr.dst_port(), guest_port);
        assert_eq!(hdr.flags(), VIRTIO_VSOCK_SHUTDOWN_F_SEND);

        notify.shutdown().unwrap();
        handle.join().unwrap();
    }
}
