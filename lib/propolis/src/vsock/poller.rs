use std::collections::{HashMap, VecDeque};
use std::ffi::c_void;
use std::io::{ErrorKind, Read};
use std::mem::MaybeUninit;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::sync::Arc;
use std::thread::JoinHandle;

use iddqd::IdHashMap;
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
use crate::vsock::proxy::{
    BackendListener, ConnKey, VsockProxyConn, CONN_TX_BUF_SIZE,
};
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
    backends: IdHashMap<BackendListener>,
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
        backends: IdHashMap<BackendListener>,
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
            backends,
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

        let Some(listner) = self.backends.get(&packet.header.dst_port()) else {
            self.rx.push_back(RxEvent::Reset {
                src_port: key.host_port,
                dst_port: key.guest_port,
            });
            return;
        };

        match VsockProxyConn::new(listner.addr()) {
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
        let key = ConnKey::from_portev_user(event.user);
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
                key.to_portev_user() as *mut c_void,
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

#[cfg(test)]
mod tests {
    use std::net::TcpListener;

    use zerocopy::FromBytes;

    use crate::accessors::MemAccessor;
    use crate::common::GuestAddr;
    use crate::hw::virtio::testutil::{Chain, DescFlag, VirtQueues, VqSize};
    use crate::hw::virtio::vsock::{VsockVq, VSOCK_RX_QUEUE};
    use crate::vmm::mem::PhysMap;
    use crate::vsock::packet::{
        VsockPacketHeader, VIRTIO_VSOCK_OP_REQUEST, VIRTIO_VSOCK_OP_RESPONSE,
        VIRTIO_VSOCK_OP_RST, VIRTIO_VSOCK_OP_RW, VIRTIO_VSOCK_TYPE_STREAM,
    };
    use crate::vsock::VSOCK_HOST_CID;

    use super::VsockPoller;

    fn test_logger() -> slog::Logger {
        use slog::Drain;
        let decorator = slog_term::TermDecorator::new().stderr().build();
        let drain = slog_term::FullFormat::new(decorator).build().fuse();
        let drain = slog_async::Async::new(drain).build().fuse();
        slog::Logger::root(drain, slog::o!("component" => "vsock-test"))
    }

    const PAGE_SIZE: u64 = 0x1000;
    const QUEUE_SIZE: u16 = 16;

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

    /// All the pieces produced by [`make_test_vsock_vq`].
    struct TestVsockVq {
        vq: VsockVq,
        mem_acc: MemAccessor,
        /// Must stay alive to keep memory mappings valid.
        _phys: PhysMap,
        /// The underlying VirtQueues, for direct queue access in assertions.
        raw_queues: VirtQueues,
        rx: QueueLayout,
        tx: QueueLayout,
        event: QueueLayout,
    }

    /// Build a VsockVq backed by test guest memory with 3 queues
    /// (RX, TX, EVENT).
    fn make_test_vsock_vq() -> TestVsockVq {
        // Lay out 3 queues sequentially
        let rx = QueueLayout::new(0, QUEUE_SIZE);
        let tx = QueueLayout::new(rx.end, QUEUE_SIZE);
        let event = QueueLayout::new(tx.end, 1);

        // Data area after all rings
        let data_area_size = PAGE_SIZE * 8;
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

        TestVsockVq { vq, mem_acc, _phys: phys, raw_queues, rx, tx, event }
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
        #[allow(dead_code)]
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

        /// Change a descriptor's flags from writable to readable so its
        /// contents can be read back via `chain.read()` after re-publishing.
        fn set_desc_readable(&self, mem_acc: &MemAccessor, idx: u16) {
            let gpa = self.layout.desc_base + u64::from(idx) * 16;
            let mem = mem_acc.access().unwrap();
            let mut raw: RawDesc = *mem.read(GuestAddr(gpa)).unwrap();
            raw.flags &= !DescFlag::WRITE.bits();
            mem.write(GuestAddr(gpa), &raw);
        }
    }

    #[test]
    fn request_receives_response() {
        // Start a TCP listener so VsockProxyConn::new() can connect
        let listener = TcpListener::bind("127.0.0.1:3000").unwrap();

        let guest_cid: u32 = 50;
        let tv = make_test_vsock_vq();

        let log = test_logger();
        let mut poller = VsockPoller::new(guest_cid, tv.vq, log).unwrap();

        // -- Populate RX queue with a writable descriptor chain --
        // The poller will need somewhere to write the RESPONSE.
        let rx_data_start = tv.event.end + PAGE_SIZE;
        let mut rx_writer = QueueWriter::new(tv.rx, rx_data_start);

        // Single writable descriptor large enough for header + data
        let d_rx = rx_writer.add_writable(&tv.mem_acc, 256);
        rx_writer.publish_avail(&tv.mem_acc, d_rx);

        // -- Build a REQUEST packet on the TX queue --
        let tx_data_start = rx_data_start + PAGE_SIZE;
        let mut tx_writer = QueueWriter::new(tv.tx, tx_data_start);

        let mut hdr = VsockPacketHeader::default();
        hdr.set_src_cid(guest_cid)
            .set_dst_cid(VSOCK_HOST_CID as u32)
            .set_src_port(1234)
            .set_dst_port(3000)
            .set_len(0)
            .set_socket_type(VIRTIO_VSOCK_TYPE_STREAM)
            .set_op(VIRTIO_VSOCK_OP_REQUEST)
            .set_buf_alloc(65536)
            .set_fwd_cnt(0);

        // Write header as a readable descriptor
        let hdr_bytes: &[u8] = unsafe {
            std::slice::from_raw_parts(
                &hdr as *const VsockPacketHeader as *const u8,
                std::mem::size_of::<VsockPacketHeader>(),
            )
        };
        let d_tx = tx_writer.add_readable(&tv.mem_acc, hdr_bytes);
        tx_writer.publish_avail(&tv.mem_acc, d_tx);

        // -- Drive the poller --
        poller.handle_tx_queue_event();
        poller.process_pending_rx();

        // -- Verify the RX queue got a RESPONSE --
        let rx_used_idx = rx_writer.used_idx(&tv.mem_acc);
        assert_eq!(rx_used_idx, 1, "expected one used entry on RX queue");

        // The poller wrote the response into the writable descriptor's
        // buffer. To read it back using pop_avail + chain.read, we flip the
        // descriptor to readable and re-publish it on the avail ring.
        rx_writer.set_desc_readable(&tv.mem_acc, d_rx);
        rx_writer.publish_avail(&tv.mem_acc, d_rx);

        let rx_vq = tv.raw_queues.get(VSOCK_RX_QUEUE).unwrap();
        let mem = tv.mem_acc.access().unwrap();
        let mut chain = Chain::with_capacity(16);
        rx_vq
            .pop_avail(&mut chain, &mem)
            .expect("re-published descriptor should be poppable");

        let mut resp_hdr = VsockPacketHeader::default();
        assert!(chain.read(&mut resp_hdr, &mem));

        assert_eq!(resp_hdr.op(), VIRTIO_VSOCK_OP_RESPONSE);
        assert_eq!(resp_hdr.src_cid(), VSOCK_HOST_CID);
        assert_eq!(resp_hdr.dst_cid(), guest_cid as u64);
        assert_eq!(resp_hdr.src_port(), 3000);
        assert_eq!(resp_hdr.dst_port(), 1234);
        assert_eq!(resp_hdr.socket_type(), VIRTIO_VSOCK_TYPE_STREAM);

        drop(listener);
    }

    #[test]
    fn rw_with_invalid_socket_type_receives_rst() {
        let guest_cid: u32 = 50;
        let tv = make_test_vsock_vq();

        let log = test_logger();
        let mut poller = VsockPoller::new(guest_cid, tv.vq, log).unwrap();

        // -- Populate RX queue with a writable descriptor for the RST --
        let rx_data_start = tv.event.end + PAGE_SIZE;
        let mut rx_writer = QueueWriter::new(tv.rx, rx_data_start);
        let d_rx = rx_writer.add_writable(&tv.mem_acc, 256);
        rx_writer.publish_avail(&tv.mem_acc, d_rx);

        // -- Build an RW packet with an invalid socket_type --
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

        let hdr_bytes: &[u8] = unsafe {
            std::slice::from_raw_parts(
                &hdr as *const VsockPacketHeader as *const u8,
                std::mem::size_of::<VsockPacketHeader>(),
            )
        };
        let d_tx = tx_writer.add_readable(&tv.mem_acc, hdr_bytes);
        tx_writer.publish_avail(&tv.mem_acc, d_tx);

        // -- Drive the poller --
        poller.handle_tx_queue_event();
        poller.process_pending_rx();

        // -- Verify the RX queue got a RST --
        let rx_used_idx = rx_writer.used_idx(&tv.mem_acc);
        assert_eq!(rx_used_idx, 1, "expected one used entry on RX queue");

        rx_writer.set_desc_readable(&tv.mem_acc, d_rx);
        rx_writer.publish_avail(&tv.mem_acc, d_rx);

        let rx_vq = tv.raw_queues.get(VSOCK_RX_QUEUE).unwrap();
        let mem = tv.mem_acc.access().unwrap();
        let mut chain = Chain::with_capacity(16);
        rx_vq
            .pop_avail(&mut chain, &mem)
            .expect("re-published descriptor should be poppable");

        let mut resp_hdr = VsockPacketHeader::default();
        assert!(chain.read(&mut resp_hdr, &mem));

        assert_eq!(resp_hdr.op(), VIRTIO_VSOCK_OP_RST);
        assert_eq!(resp_hdr.src_cid(), VSOCK_HOST_CID);
        assert_eq!(resp_hdr.dst_cid(), guest_cid as u64);
        assert_eq!(resp_hdr.src_port(), 8080);
        assert_eq!(resp_hdr.dst_port(), 5555);
    }
}
