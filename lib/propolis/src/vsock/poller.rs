// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::collections::VecDeque;
use std::ffi::c_void;
use std::io::ErrorKind;
use std::io::Read;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::sync::Arc;
use std::thread::JoinHandle;

use iddqd::IdHashMap;
use nix::poll::PollFlags;
use slog::{debug, error, info, warn, Logger};

use crate::hw::virtio::vsock::VsockVq;
use crate::hw::virtio::vsock::VSOCK_RX_QUEUE;
use crate::hw::virtio::vsock::VSOCK_TX_QUEUE;
use crate::vsock::packet::VsockPacket;
use crate::vsock::packet::VsockPacketFlags;
use crate::vsock::packet::VsockSocketType;
use crate::vsock::proxy::ConnKey;
use crate::vsock::proxy::VsockPortMapping;
use crate::vsock::proxy::VsockProxyConn;
use crate::vsock::VSOCK_HOST_CID;

use super::packet::VsockPacketOp;

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

/// Set of `PollFlags` that signifies a readable event.
const fn is_readable(flags: PollFlags) -> bool {
    const READABLE: PollFlags = PollFlags::from_bits_truncate(
        PollFlags::POLLIN.bits()
            | PollFlags::POLLHUP.bits()
            | PollFlags::POLLERR.bits()
            | PollFlags::POLLPRI.bits(),
    );
    READABLE.intersects(flags)
}

/// Set of `PollFlags` that signifies a writable event.
const fn is_writable(flags: PollFlags) -> bool {
    const WRITABLE: PollFlags = PollFlags::from_bits_truncate(
        PollFlags::POLLOUT.bits()
            | PollFlags::POLLHUP.bits()
            | PollFlags::POLLERR.bits(),
    );
    WRITABLE.intersects(flags)
}

#[derive(Debug)]
enum RxEvent {
    /// Vsock RST packet
    Reset(ConnKey),
    /// Vsock RESPONSE packet
    NewConnection(ConnKey),
    /// Vsock CREDIT_UPDATE packet
    CreditUpdate(ConnKey),
}

pub struct VsockPoller {
    log: Logger,
    /// The guest context id
    guest_cid: u32,
    /// Port mappings we are proxying packets to and from
    port_mappings: IdHashMap<VsockPortMapping>,
    /// The event port fd.
    port_fd: Arc<OwnedFd>,
    /// The virtqueues associated with the vsock device
    queues: VsockVq,
    /// The connection map of guest connected streams
    connections: HashMap<ConnKey, VsockProxyConn>,
    /// Queue of vsock packets that need to be sent to the guest
    rx: VecDeque<RxEvent>,
    /// Connections blocked waiting for rx queue descriptors
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
    fn handle_shutdown(&mut self, key: ConnKey, flags: VsockPacketFlags) {
        if let Entry::Occupied(mut entry) = self.connections.entry(key) {
            let conn = entry.get_mut();

            // Guest won't receive more data
            if flags.contains(VsockPacketFlags::VIRTIO_VSOCK_SHUTDOWN_F_RECEIVE)
            {
                if let Err(e) = conn.shutdown_guest_read() {
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
            if flags.contains(VsockPacketFlags::VIRTIO_VSOCK_SHUTDOWN_F_SEND) {
                if let Err(e) = conn.shutdown_guest_write() {
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
        if let Entry::Occupied(mut entry) = self.connections.entry(key) {
            let conn = entry.get_mut();

            // If we have a valid connection attempt to consume the guest's
            // packet.
            if let Err(e) = conn.recv_packet(packet) {
                error!(
                    &self.log,
                    "failed to push vsock packet data into the conn vbuf: {e}";
                    "conn" => ?conn,
                );

                entry.remove();
                self.send_conn_rst(key);
                return;
            }

            if let Some(interests) = conn.poll_interests() {
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

            // If the packet is not coming from our guest drop it.
            if packet.header.src_cid() != u64::from(self.guest_cid) {
                // Note that we could send a RST here but technically we should
                // not know how to address this guest cid as it's not the one
                // we assigned to our guest.
                debug!(
                    &self.log,
                    "droppping vsock packet not arriving from our guest cid";
                    "packet" => ?packet,
                );
                continue;
            }

            let key = ConnKey {
                host_port: packet.header.dst_port(),
                guest_port: packet.header.src_port(),
            };

            // We only support stream connections
            let Some(VsockSocketType::Stream) = packet.header.socket_type()
            else {
                self.send_conn_rst(key);
                warn!(&self.log,
                    "received invalid vsock packet";
                    "packet" => ?packet,
                );
                continue;
            };

            let Some(packet_op) = packet.header.op() else {
                warn!(
                    &self.log,
                    "received vsock packet with unknown op code";
                    "packet" => ?packet,
                );
                return;
            };

            if let Some(conn) = self.connections.get_mut(&key) {
                // Regardless of the vsock operation we need to record the peers
                // credit info
                conn.update_peer_credit(&packet.header);
                match packet_op {
                    VsockPacketOp::Reset => {
                        self.connections.remove(&key);
                    }
                    VsockPacketOp::Shutdown => {
                        self.handle_shutdown(key, packet.header.flags());
                    }
                    VsockPacketOp::CreditUpdate => continue,
                    VsockPacketOp::CreditRequest => {
                        if self.connections.contains_key(&key) {
                            self.rx.push_back(RxEvent::CreditUpdate(key));
                        }
                    }
                    VsockPacketOp::ReadWrite => {
                        self.handle_rw_packet(key, packet);
                    }
                    // We are operating on an existing connection either of
                    // these should not be received
                    //
                    // XXX: send a RST, but what about our orignal connection?
                    VsockPacketOp::Request | VsockPacketOp::Response => (),
                }
            } else {
                match packet_op {
                    VsockPacketOp::Request => {
                        self.handle_connection_request(key, packet)
                    }
                    VsockPacketOp::Reset => {}
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

    /// Process the rx virtqueue (host -> guest).
    fn handle_rx_queue_event(&mut self) {
        // Now that more descriptors have become available for sending vsock
        // packets attempt to drain pending packets
        self.process_pending_rx();

        // Re-register connections that were blocked waiting for rx queue space.
        // It would be nice if we had a hint of how many descriptors became
        // available but that's not the case today.
        for key in std::mem::take(&mut self.rx_blocked).drain(..) {
            if let Some(conn) = self.connections.get(&key) {
                if let Some(interests) = conn.poll_interests() {
                    let fd = conn.get_fd();
                    self.associate_fd(key, fd, interests);
                }
            }
        }
    }

    // Attempt to send any queued rx packets destined for the guest.
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

                        if let Some(interests) = conn.poll_interests() {
                            let fd = conn.get_fd();
                            self.associate_fd(key, fd, interests);
                        }
                    }
                }
                RxEvent::CreditUpdate(key) => {
                    if let Some(conn) = self.connections.get_mut(&key) {
                        let packet = VsockPacket::new_credit_update(
                            self.guest_cid,
                            key.host_port,
                            key.guest_port,
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

    /// Handle an fd event by flushing data to the underlying socket from the
    /// connections [`VsockBuf`], and by reading data from the socket and
    /// sending it to the guest as a `VIRTIO_VSOCK_OP_RW` packet.
    fn handle_fd_event(&mut self, event: PortEvent, read_buf: &mut [u8]) {
        let key = ConnKey::from_portev_user(event.user);
        let events = PollFlags::from_bits_retain(event.events as i16);

        if is_writable(events) {
            self.handle_writable_fd(key);
        }

        if is_readable(events) {
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
            self.connections.remove(&key);
            self.send_conn_rst(key);
            return;
        }

        if let Some(interests) = conn.poll_interests() {
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
        if !conn.guest_can_read() {
            return;
        }

        loop {
            let Some(permit) = queues.try_rx_permit() else {
                rx_blocked.push(key);
                break;
            };

            let credit = conn.peer_credit();
            if credit == 0 {
                // TODO: when this happens under sufficient load there's the
                // possibility we wake up the event loop repeatedly and we
                // should defer associating this fd again until there's enough
                // credit. This is similar to the `rx_blocked` queue but
                // slightly different.
                break;
            }

            let max_read = std::cmp::min(
                permit.available_data_space(),
                std::cmp::min(credit as usize, read_buf.len()),
            );

            match conn.socket.read(&mut read_buf[..max_read]) {
                Ok(0) => {
                    // TODO the guest is supposed to send us a RST to finalize
                    // the shutdown. We need to put this on a quiesce queue so
                    // that we don't leave a half open connection laying around
                    // in our connection map.
                    let packet = VsockPacket::new_shutdown(
                        *guest_cid,
                        key.host_port,
                        key.guest_port,
                        VsockPacketFlags::VIRTIO_VSOCK_SHUTDOWN_F_SEND
                            | VsockPacketFlags::VIRTIO_VSOCK_SHUTDOWN_F_RECEIVE,
                        conn.fwd_cnt(),
                    );
                    permit.write(&packet.header, &packet.data);
                    return;
                }
                Ok(nbytes) => {
                    let read_u32: u32 = nbytes
                        .try_into()
                        .expect("max_read is <=u32::MAX by min() above");
                    conn.update_tx_cnt(read_u32);
                    let VsockPacket { header, data } = VsockPacket::new_rw(
                        *guest_cid,
                        key.host_port,
                        key.guest_port,
                        conn.fwd_cnt(),
                        &read_buf[..nbytes],
                    );
                    permit.write(&header, &data);
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

        if let Some(interests) = conn.poll_interests() {
            let fd = conn.get_fd();
            self.associate_fd(key, fd, interests);
        }
    }

    /// Associate a connections underlying socket fd with our port fd.
    fn associate_fd(&mut self, key: ConnKey, fd: RawFd, interests: PollFlags) {
        let ret = unsafe {
            libc::port_associate(
                self.port_fd.as_raw_fd(),
                libc::PORT_SOURCE_FD,
                fd as usize,
                interests.bits() as i32,
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

    /// This is the vsock event-loop. It's responsible for handling vsock
    /// packets to and from the guest.
    fn handle_events(&mut self) {
        const MAX_EVENTS: u32 = 32;

        let mut events = [const { unsafe { std::mem::zeroed::<libc::port_event>() } };
            MAX_EVENTS as usize];
        let mut read_buf: Box<[u8]> = vec![0u8; 1024 * 64].into();

        loop {
            let mut nget = 1;

            let ret = unsafe {
                libc::port_getn(
                    self.port_fd.as_raw_fd(),
                    events.as_mut_ptr(),
                    MAX_EVENTS,
                    &mut nget,
                    // TODO currently we are not supplying a timeout because
                    // there is no other work to do unless we are woken up. In
                    // the near future we will likely periodically wake up to
                    // service the shutdown quiesce queue.
                    std::ptr::null_mut(),
                )
            };

            if ret < 0 {
                let err = std::io::Error::last_os_error();
                // SAFETY: The docs state that `raw_os_error` will always return
                // a `Some` variant when obtained via `last_os_error`.
                match err.raw_os_error().unwrap() {
                    // A signal was caught so process the loop again
                    libc::EINTR => continue,
                    libc::EBADF | libc::EBADFD => {
                        // This means our event loop is effectively no
                        // longer servicable and the vsock device is useless.
                        error!(
                            &self.log,
                            "vsock port fd is no longer valid: {err}"
                        );
                        return;
                    }
                    _ => {
                        error!(&self.log, "vsock port_getn returned: {err}");
                        continue;
                    }
                }
            }

            assert!(
                nget as usize <= events.len(),
                "event port returned what we asked it for"
            );
            let events = unsafe {
                std::slice::from_raw_parts(events.as_ptr(), nget as usize)
            };
            for event in events {
                let event = PortEvent::from_raw(*event);

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

    use zerocopy::{FromBytes, IntoBytes};

    use crate::hw::virtio::testutil::{QueueWriter, TestVirtQueues, VqSize};
    use crate::hw::virtio::vsock::{VsockVq, VSOCK_RX_QUEUE, VSOCK_TX_QUEUE};
    use crate::vsock::packet::{
        VsockPacketFlags, VsockPacketHeader, VsockPacketOp, VsockSocketType,
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
    /// `IdHashMap<VsockPortMapping>` that maps `vsock_port` to the listener's
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

    /// Test harness for vsock poller tests using shared testutil infrastructure.
    struct VsockTestHarness {
        tvqs: TestVirtQueues,
        rx_writer: QueueWriter,
        tx_writer: QueueWriter,
    }

    impl VsockTestHarness {
        fn new() -> Self {
            let tvqs = TestVirtQueues::new(&[
                VqSize::new(QUEUE_SIZE), // RX
                VqSize::new(QUEUE_SIZE), // TX
                VqSize::new(1),          // Event
            ]);

            // RX and TX use separate data regions
            let rx_writer = tvqs.writer(VSOCK_RX_QUEUE as usize, 0);
            let tx_writer =
                tvqs.writer(VSOCK_TX_QUEUE as usize, PAGE_SIZE * 16);

            Self { tvqs, rx_writer, tx_writer }
        }

        fn make_vsock_vq(&self) -> VsockVq {
            let queues: Vec<_> =
                self.tvqs.queues().iter().map(|q| q.clone()).collect();
            let acc = self.tvqs.mem_acc().child(Some("vsock-vq".to_string()));
            VsockVq::new(queues, acc)
        }

        /// Add a writable descriptor to the RX queue and publish it.
        fn add_rx_writable(&mut self, len: u32) -> u16 {
            let d = self.rx_writer.add_writable(self.tvqs.mem_acc(), len);
            self.rx_writer.publish_avail(self.tvqs.mem_acc(), d);
            d
        }

        /// Add a readable descriptor to the TX queue.
        fn add_tx_readable(&mut self, data: &[u8]) -> u16 {
            self.tx_writer.add_readable(self.tvqs.mem_acc(), data)
        }

        /// Publish a descriptor on the TX queue.
        fn publish_tx(&mut self, head: u16) {
            self.tx_writer.publish_avail(self.tvqs.mem_acc(), head);
        }

        /// Chain two TX descriptors together.
        fn chain_tx(&mut self, from: u16, to: u16) {
            self.tx_writer.chain(self.tvqs.mem_acc(), from, to);
        }

        /// Reset TX writer cursors for reuse.
        fn reset_tx_cursors(&mut self) {
            self.tx_writer.reset_cursors();
        }

        /// Reset RX writer cursors for reuse.
        fn reset_rx_cursors(&mut self) {
            self.rx_writer.reset_cursors();
        }

        /// Read a vsock packet header and data from a used ring entry.
        fn read_vsock_packet(
            &self,
            used_index: u16,
        ) -> (VsockPacketHeader, Vec<u8>) {
            let mem_acc = self.tvqs.mem_acc();
            let elem = self.rx_writer.read_used_elem(mem_acc, used_index);
            let desc_id = elem.id as u16;
            let total_len = elem.len as usize;

            // Read the entire buffer (header + data)
            let buf =
                self.rx_writer.read_desc_data(mem_acc, desc_id, total_len);

            // Parse header from the first bytes
            let hdr_size = std::mem::size_of::<VsockPacketHeader>();
            let (hdr, data) = buf.split_at(hdr_size);
            let hdr = VsockPacketHeader::read_from_bytes(hdr)
                .expect("buffer should contain valid header");

            (hdr, data.to_vec())
        }

        fn rx_used_idx(&self) -> u16 {
            self.rx_writer.used_idx(self.tvqs.mem_acc())
        }

        fn tx_used_idx(&self) -> u16 {
            self.tx_writer.used_idx(self.tvqs.mem_acc())
        }
    }

    /// Helper: serialize a VsockPacketHeader to bytes.
    fn hdr_as_bytes(hdr: &VsockPacketHeader) -> &[u8] {
        hdr.as_bytes()
    }

    /// Spin until a condition is met, with a timeout.
    fn wait_for_condition<F>(mut f: F, timeout_ms: u64)
    where
        F: FnMut() -> bool,
    {
        let start = std::time::Instant::now();
        let timeout = Duration::from_millis(timeout_ms);
        while !f() {
            if start.elapsed() > timeout {
                panic!("timed out waiting for condition");
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn request_receives_response() {
        let vsock_port = 3000;
        let guest_port = 1234;
        let guest_cid: u32 = 50;
        let (_listener, backends) = bind_test_backend(vsock_port);

        let mut harness = VsockTestHarness::new();
        let vq = harness.make_vsock_vq();
        let log = test_logger();
        let poller = VsockPoller::new(guest_cid, vq, log, backends).unwrap();

        harness.add_rx_writable(256);

        let notify = poller.notify_handle();
        let handle = poller.run();

        let mut hdr = VsockPacketHeader::new();
        hdr.set_src_cid(guest_cid)
            .set_dst_cid(VSOCK_HOST_CID as u32)
            .set_src_port(guest_port)
            .set_dst_port(vsock_port)
            .set_len(0)
            .set_socket_type(VsockSocketType::Stream)
            .set_op(crate::vsock::packet::VsockPacketOp::Request)
            .set_buf_alloc(65536)
            .set_fwd_cnt(0);

        let d_tx = harness.add_tx_readable(hdr_as_bytes(&hdr));
        harness.publish_tx(d_tx);
        notify.queue_notify(VSOCK_TX_QUEUE).unwrap();

        wait_for_condition(|| harness.rx_used_idx() >= 1, 5000);

        let (resp_hdr, _) = harness.read_vsock_packet(0);
        assert_eq!(resp_hdr.op(), Some(VsockPacketOp::Response));
        assert_eq!(resp_hdr.src_cid(), VSOCK_HOST_CID);
        assert_eq!(resp_hdr.dst_cid(), guest_cid as u64);
        assert_eq!(resp_hdr.src_port(), vsock_port);
        assert_eq!(resp_hdr.dst_port(), guest_port);
        assert_eq!(resp_hdr.socket_type(), Some(VsockSocketType::Stream));

        notify.shutdown().unwrap();
        handle.join().unwrap();
    }

    #[test]
    fn rw_with_invalid_socket_type_receives_rst() {
        let guest_cid: u32 = 50;

        let mut harness = VsockTestHarness::new();
        let vq = harness.make_vsock_vq();
        let log = test_logger();
        let poller =
            VsockPoller::new(guest_cid, vq, log, IdHashMap::new()).unwrap();

        harness.add_rx_writable(256);

        let notify = poller.notify_handle();
        let handle = poller.run();

        let mut hdr = VsockPacketHeader::new();
        hdr.set_src_cid(guest_cid)
            .set_dst_cid(VSOCK_HOST_CID as u32)
            .set_src_port(5555)
            .set_dst_port(8080)
            .set_len(0)
            .set_socket_type(VsockSocketType::InvalidTestValue)
            .set_op(VsockPacketOp::ReadWrite)
            .set_buf_alloc(65536)
            .set_fwd_cnt(0);

        let d_tx = harness.add_tx_readable(hdr_as_bytes(&hdr));
        harness.publish_tx(d_tx);
        notify.queue_notify(VSOCK_TX_QUEUE).unwrap();

        wait_for_condition(|| harness.rx_used_idx() >= 1, 5000);

        let (resp_hdr, _) = harness.read_vsock_packet(0);
        assert_eq!(resp_hdr.op(), Some(VsockPacketOp::Reset));
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

        let mut harness = VsockTestHarness::new();
        let vq = harness.make_vsock_vq();
        let log = test_logger();
        let poller = VsockPoller::new(guest_cid, vq, log, backends).unwrap();

        for _ in 0..4 {
            harness.add_rx_writable(4096);
        }

        let notify = poller.notify_handle();
        let handle = poller.run();

        // Send REQUEST
        let mut req_hdr = VsockPacketHeader::new();
        req_hdr
            .set_src_cid(guest_cid)
            .set_dst_cid(VSOCK_HOST_CID as u32)
            .set_src_port(guest_port)
            .set_dst_port(vsock_port)
            .set_len(0)
            .set_socket_type(VsockSocketType::Stream)
            .set_op(VsockPacketOp::Request)
            .set_buf_alloc(65536)
            .set_fwd_cnt(0);

        let d_tx = harness.add_tx_readable(hdr_as_bytes(&req_hdr));
        harness.publish_tx(d_tx);
        notify.queue_notify(VSOCK_TX_QUEUE).unwrap();

        // Accept TCP connection and wait for RESPONSE
        let mut accepted = listener.accept().unwrap().0;
        accepted.set_nonblocking(false).unwrap();
        accepted.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        wait_for_condition(|| harness.rx_used_idx() >= 1, 5000);

        // Send RW packet with data payload
        let payload = b"hello from guest via vsock!";
        let mut rw_hdr = VsockPacketHeader::new();
        rw_hdr
            .set_src_cid(guest_cid)
            .set_dst_cid(VSOCK_HOST_CID as u32)
            .set_src_port(guest_port)
            .set_dst_port(vsock_port)
            .set_len(payload.len() as u32)
            .set_socket_type(VsockSocketType::Stream)
            .set_op(VsockPacketOp::ReadWrite)
            .set_buf_alloc(65536)
            .set_fwd_cnt(0);

        let d_hdr = harness.add_tx_readable(hdr_as_bytes(&rw_hdr));
        let d_body = harness.add_tx_readable(payload);
        harness.chain_tx(d_hdr, d_body);
        harness.publish_tx(d_hdr);
        notify.queue_notify(VSOCK_TX_QUEUE).unwrap();

        // Read from accepted TCP stream and verify
        let mut buf = vec![0u8; payload.len()];
        accepted.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, payload);

        notify.shutdown().unwrap();
        handle.join().unwrap();
    }

    #[test]
    fn credit_update_sent_after_flushing_half_buffer() {
        let vsock_port = 4000;
        let guest_port = 2000;
        let guest_cid: u32 = 50;
        let (listener, backends) = bind_test_backend(vsock_port);
        listener.set_nonblocking(false).unwrap();

        let mut harness = VsockTestHarness::new();
        let vq = harness.make_vsock_vq();
        let log = test_logger();
        let poller = VsockPoller::new(guest_cid, vq, log, backends).unwrap();

        // Provide plenty of RX descriptors for RESPONSE + credit updates
        for _ in 0..16 {
            harness.add_rx_writable(4096);
        }

        let notify = poller.notify_handle();
        let handle = poller.run();

        // Establish connection
        let mut req_hdr = VsockPacketHeader::new();
        req_hdr
            .set_src_cid(guest_cid)
            .set_dst_cid(VSOCK_HOST_CID as u32)
            .set_src_port(guest_port)
            .set_dst_port(vsock_port)
            .set_len(0)
            .set_socket_type(VsockSocketType::Stream)
            .set_op(VsockPacketOp::Request)
            .set_buf_alloc(65536)
            .set_fwd_cnt(0);

        let d_tx = harness.add_tx_readable(hdr_as_bytes(&req_hdr));
        harness.publish_tx(d_tx);
        notify.queue_notify(VSOCK_TX_QUEUE).unwrap();

        let mut accepted = listener.accept().unwrap().0;
        accepted.set_nonblocking(false).unwrap();
        accepted.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        wait_for_condition(|| harness.rx_used_idx() >= 1, 5000);

        // Send enough data to exceed half the buffer capacity (64KB).
        let chunk_size = 8192;
        let num_chunks = (CONN_TX_BUF_SIZE / 2) / chunk_size + 1;
        let payload = vec![0xAB_u8; chunk_size];
        let total_sent = num_chunks * chunk_size;
        let mut tx_consumed = 1u16; // REQUEST was consumed

        for _ in 0..num_chunks {
            // Reuse descriptor slots each iteration
            harness.reset_tx_cursors();

            let mut rw_hdr = VsockPacketHeader::new();
            rw_hdr
                .set_src_cid(guest_cid)
                .set_dst_cid(VSOCK_HOST_CID as u32)
                .set_src_port(guest_port)
                .set_dst_port(vsock_port)
                .set_len(payload.len() as u32)
                .set_socket_type(VsockSocketType::Stream)
                .set_op(VsockPacketOp::ReadWrite)
                .set_buf_alloc(65536)
                .set_fwd_cnt(0);

            let d_hdr = harness.add_tx_readable(hdr_as_bytes(&rw_hdr));
            let d_body = harness.add_tx_readable(&payload);
            harness.chain_tx(d_hdr, d_body);
            harness.publish_tx(d_hdr);
            notify.queue_notify(VSOCK_TX_QUEUE).unwrap();

            tx_consumed += 1;
            wait_for_condition(|| harness.tx_used_idx() >= tx_consumed, 5000);
        }

        // Drain the data from the accepted socket to confirm it arrived
        let mut buf = vec![0u8; total_sent];
        accepted.read_exact(&mut buf).unwrap();
        assert!(buf.iter().all(|&b| b == 0xAB));

        // Look for a CREDIT_UPDATE in the RX used entries
        let rx_used = harness.rx_used_idx();
        assert!(rx_used >= 2, "expected at least RESPONSE + CREDIT_UPDATE");

        let mut found_credit_update = false;
        for i in 1..rx_used {
            let (hdr, _) = harness.read_vsock_packet(i);
            if hdr.op() == Some(VsockPacketOp::CreditUpdate) {
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

        let mut harness = VsockTestHarness::new();
        let vq = harness.make_vsock_vq();
        let log = test_logger();
        let poller = VsockPoller::new(guest_cid, vq, log, backends).unwrap();

        for _ in 0..4 {
            harness.add_rx_writable(4096);
        }

        let notify = poller.notify_handle();
        let handle = poller.run();

        // Send REQUEST
        let mut req_hdr = VsockPacketHeader::new();
        req_hdr
            .set_src_cid(guest_cid)
            .set_dst_cid(VSOCK_HOST_CID as u32)
            .set_src_port(guest_port)
            .set_dst_port(vsock_port)
            .set_len(0)
            .set_socket_type(VsockSocketType::Stream)
            .set_op(VsockPacketOp::Request)
            .set_buf_alloc(65536)
            .set_fwd_cnt(0);

        let d_tx = harness.add_tx_readable(hdr_as_bytes(&req_hdr));
        harness.publish_tx(d_tx);
        notify.queue_notify(VSOCK_TX_QUEUE).unwrap();

        let mut accepted = listener.accept().unwrap().0;
        accepted.set_nonblocking(false).unwrap();
        accepted.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        wait_for_condition(|| harness.rx_used_idx() >= 1, 5000);

        // Send RST
        let mut rst_hdr = VsockPacketHeader::new();
        rst_hdr
            .set_src_cid(guest_cid)
            .set_dst_cid(VSOCK_HOST_CID as u32)
            .set_src_port(guest_port)
            .set_dst_port(vsock_port)
            .set_len(0)
            .set_socket_type(VsockSocketType::Stream)
            .set_op(VsockPacketOp::Reset)
            .set_buf_alloc(0)
            .set_fwd_cnt(0);

        let d_rst = harness.add_tx_readable(hdr_as_bytes(&rst_hdr));
        harness.publish_tx(d_rst);
        notify.queue_notify(VSOCK_TX_QUEUE).unwrap();

        // Wait for the RST to be consumed
        wait_for_condition(|| harness.tx_used_idx() >= 2, 5000);

        // Verify the TCP connection was closed by reading from the
        // accepted stream.
        let mut buf = [0u8; 1];
        let result = accepted.read(&mut buf);
        match result {
            Ok(0) => {}
            Err(_) => {}
            Ok(n) => panic!("expected EOF or error, got {n} bytes"),
        }

        notify.shutdown().unwrap();
        handle.join().unwrap();
    }

    #[test]
    fn end_to_end_guest_to_host() {
        let vsock_port = 7000;
        let guest_port = 5000;
        let guest_cid: u32 = 50;
        let (listener, backends) = bind_test_backend(vsock_port);
        listener.set_nonblocking(false).unwrap();

        let mut harness = VsockTestHarness::new();
        let vq = harness.make_vsock_vq();
        let log = test_logger();
        let poller = VsockPoller::new(guest_cid, vq, log, backends).unwrap();

        // Pre-populate RX queue with writable descriptors for RESPONSE + data
        for _ in 0..8 {
            harness.add_rx_writable(4096);
        }

        let notify = poller.notify_handle();
        let handle = poller.run();

        // Write REQUEST packet into TX queue
        let mut req_hdr = VsockPacketHeader::new();
        req_hdr
            .set_src_cid(guest_cid)
            .set_dst_cid(VSOCK_HOST_CID as u32)
            .set_src_port(guest_port)
            .set_dst_port(vsock_port)
            .set_len(0)
            .set_socket_type(VsockSocketType::Stream)
            .set_op(VsockPacketOp::Request)
            .set_buf_alloc(65536)
            .set_fwd_cnt(0);

        let d_tx = harness.add_tx_readable(hdr_as_bytes(&req_hdr));
        harness.publish_tx(d_tx);
        notify.queue_notify(VSOCK_TX_QUEUE).unwrap();

        // Accept the TCP connection (blocks until poller connects)
        let mut accepted = listener.accept().unwrap().0;
        accepted.set_nonblocking(false).unwrap();
        accepted.set_read_timeout(Some(Duration::from_secs(5))).unwrap();

        // Wait for RESPONSE on RX queue
        wait_for_condition(|| harness.rx_used_idx() >= 1, 5000);

        // Guest->Host: send RW packet with payload
        let payload = b"hello from guest via vsock end-to-end!";
        let mut rw_hdr = VsockPacketHeader::new();
        rw_hdr
            .set_src_cid(guest_cid)
            .set_dst_cid(VSOCK_HOST_CID as u32)
            .set_src_port(guest_port)
            .set_dst_port(vsock_port)
            .set_len(payload.len() as u32)
            .set_socket_type(VsockSocketType::Stream)
            .set_op(VsockPacketOp::ReadWrite)
            .set_buf_alloc(65536)
            .set_fwd_cnt(0);

        let d_hdr = harness.add_tx_readable(hdr_as_bytes(&rw_hdr));
        let d_body = harness.add_tx_readable(payload);
        harness.chain_tx(d_hdr, d_body);
        harness.publish_tx(d_hdr);
        notify.queue_notify(VSOCK_TX_QUEUE).unwrap();

        // Read from accepted TCP stream, and verify guest->host data
        let mut buf = vec![0u8; payload.len()];
        accepted.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, payload, "guest->host data mismatch");

        // Host->Guest: write data into accepted TCP stream
        let host_payload = b"reply from host via vsock!";
        accepted.write_all(host_payload).unwrap();
        accepted.flush().unwrap();

        // Wait for RW packet on RX queue (RESPONSE was 1, now expect 2+)
        wait_for_condition(|| harness.rx_used_idx() >= 2, 5000);

        // Read back the RW packet from RX used ring entry 1
        let (resp_hdr, host_buf) = harness.read_vsock_packet(1);

        assert_eq!(resp_hdr.op(), Some(VsockPacketOp::ReadWrite));
        assert_eq!(resp_hdr.src_port(), vsock_port);
        assert_eq!(resp_hdr.dst_port(), guest_port);
        assert_eq!(&host_buf, host_payload, "host->guest data mismatch");

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

        let mut harness = VsockTestHarness::new();
        let vq = harness.make_vsock_vq();
        let log = test_logger();
        let poller = VsockPoller::new(guest_cid, vq, log, backends).unwrap();

        // Provide only one RX descriptor, just enough for the RESPONSE.
        harness.add_rx_writable(4096);

        let notify = poller.notify_handle();
        let handle = poller.run();

        // Send REQUEST
        let mut req_hdr = VsockPacketHeader::new();
        req_hdr
            .set_src_cid(guest_cid)
            .set_dst_cid(VSOCK_HOST_CID as u32)
            .set_src_port(guest_port)
            .set_dst_port(vsock_port)
            .set_len(0)
            .set_socket_type(VsockSocketType::Stream)
            .set_op(VsockPacketOp::Request)
            .set_buf_alloc(65536)
            .set_fwd_cnt(0);

        let d_tx = harness.add_tx_readable(hdr_as_bytes(&req_hdr));
        harness.publish_tx(d_tx);
        notify.queue_notify(VSOCK_TX_QUEUE).unwrap();

        let mut accepted = listener.accept().unwrap().0;
        accepted.set_nonblocking(false).unwrap();
        wait_for_condition(|| harness.rx_used_idx() >= 1, 5000);

        // The RESPONSE consumed the only RX descriptor. Write data from
        // the host side.
        let host_data = b"data from the host side";
        accepted.write_all(host_data).unwrap();
        accepted.flush().unwrap();

        // Give the poller time to attempt delivery (and get blocked)
        std::thread::sleep(Duration::from_millis(100));

        // Verify no new used entries appeared (still just the RESPONSE)
        assert_eq!(harness.rx_used_idx(), 1);

        // Add new RX descriptors and notify
        harness.reset_rx_cursors();
        harness.add_rx_writable(4096);
        notify.queue_notify(VSOCK_RX_QUEUE).unwrap();

        // Wait for the data to be delivered
        wait_for_condition(|| harness.rx_used_idx() >= 2, 5000);

        let (rw_hdr, payload) = harness.read_vsock_packet(1);
        assert_eq!(rw_hdr.op(), Some(VsockPacketOp::ReadWrite));
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

        let mut harness = VsockTestHarness::new();
        let vq = harness.make_vsock_vq();
        let log = test_logger();
        let poller = VsockPoller::new(guest_cid, vq, log, backends).unwrap();

        // Provide initial RX descriptors for RESPONSE + credit updates
        for _ in 0..8 {
            harness.add_rx_writable(4096);
        }

        let notify = poller.notify_handle();
        let handle = poller.run();

        // Establish connection
        // Use a large buf_alloc so host->guest credit doesn't run out
        // before we've transferred all the data.
        let buf_alloc = total_bytes as u32 * 2;

        let mut req_hdr = VsockPacketHeader::new();
        req_hdr
            .set_src_cid(guest_cid)
            .set_dst_cid(VSOCK_HOST_CID as u32)
            .set_src_port(guest_port)
            .set_dst_port(vsock_port)
            .set_len(0)
            .set_socket_type(VsockSocketType::Stream)
            .set_op(VsockPacketOp::Request)
            .set_buf_alloc(buf_alloc)
            .set_fwd_cnt(0);

        let d_tx = harness.add_tx_readable(hdr_as_bytes(&req_hdr));
        harness.publish_tx(d_tx);
        notify.queue_notify(VSOCK_TX_QUEUE).unwrap();

        let accepted = listener.accept().unwrap().0;
        accepted.set_nonblocking(false).unwrap();

        wait_for_condition(|| harness.rx_used_idx() >= 1, 5000);

        // A reader thread drains the TCP socket while the main thread
        // injects RW packets in batches, reusing descriptor slots and
        // guest memory between batches.
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
            harness.reset_tx_cursors();

            for i in 0..this_batch {
                let offset = guest_sent + i * chunk_size;
                let end = std::cmp::min(offset + chunk_size, total_bytes);
                let payload = &guest_data[offset..end];

                let mut rw_hdr = VsockPacketHeader::new();
                rw_hdr
                    .set_src_cid(guest_cid)
                    .set_dst_cid(VSOCK_HOST_CID as u32)
                    .set_src_port(guest_port)
                    .set_dst_port(vsock_port)
                    .set_len(payload.len() as u32)
                    .set_socket_type(VsockSocketType::Stream)
                    .set_op(VsockPacketOp::ReadWrite)
                    .set_buf_alloc(buf_alloc)
                    .set_fwd_cnt(0);

                let d_hdr = harness.add_tx_readable(hdr_as_bytes(&rw_hdr));
                let d_body = harness.add_tx_readable(payload);
                harness.chain_tx(d_hdr, d_body);
                harness.publish_tx(d_hdr);
            }

            notify.queue_notify(VSOCK_TX_QUEUE).unwrap();

            // Wait for the poller to consume this entire batch before
            // we overwrite the descriptor slots in the next iteration.
            tx_consumed += this_batch as u16;
            wait_for_condition(|| harness.tx_used_idx() >= tx_consumed, 10000);

            guest_sent += this_batch * chunk_size;
            if guest_sent > total_bytes {
                guest_sent = total_bytes;
            }
        }

        let received = tcp_reader.join().unwrap();
        assert_eq!(received.len(), total_bytes);
        assert!(received == guest_data, "guest->host data mismatch");

        // A writer thread pushes data into the TCP socket while the
        // main thread replenishes RX descriptors in batches, reads
        // completed used entries, and reuses descriptor slots once
        // the entire batch has been consumed.
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
        let mut rx_next_used = harness.rx_used_idx();
        let rx_batch = 16u16;
        let mut descs_outstanding = 0u16;

        while host_to_guest.len() < total_bytes {
            // When all outstanding descriptors have been consumed we can
            // safely reuse the descriptor slots and data region.
            if descs_outstanding == 0 {
                harness.reset_rx_cursors();

                for _ in 0..rx_batch {
                    harness.add_rx_writable(4096);
                    descs_outstanding += 1;
                }
                notify.queue_notify(VSOCK_RX_QUEUE).unwrap();
            }

            // Wait for at least one new used entry.
            wait_for_condition(|| harness.rx_used_idx() > rx_next_used, 10000);

            // Drain all currently available used entries.
            let current_used = harness.rx_used_idx();
            while rx_next_used < current_used {
                let (hdr, data) = harness.read_vsock_packet(rx_next_used);
                rx_next_used += 1;
                descs_outstanding -= 1;

                if hdr.op() == Some(VsockPacketOp::ReadWrite) {
                    host_to_guest.extend_from_slice(&data);
                }
                // Credit updates and other control packets are
                // silently consumed — they're expected here.
            }
        }

        tcp_writer.join().unwrap();
        assert_eq!(host_to_guest.len(), total_bytes);
        assert!(host_to_guest == host_data, "host->guest data mismatch");

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

        let mut harness = VsockTestHarness::new();
        let vq = harness.make_vsock_vq();
        let log = test_logger();
        let poller = VsockPoller::new(guest_cid, vq, log, backends).unwrap();

        // Provide RX descriptors for RESPONSE + SHUTDOWN
        for _ in 0..4 {
            harness.add_rx_writable(4096);
        }

        let notify = poller.notify_handle();
        let handle = poller.run();

        // Establish connection
        let mut req_hdr = VsockPacketHeader::new();
        req_hdr
            .set_src_cid(guest_cid)
            .set_dst_cid(VSOCK_HOST_CID as u32)
            .set_src_port(guest_port)
            .set_dst_port(vsock_port)
            .set_len(0)
            .set_socket_type(VsockSocketType::Stream)
            .set_op(VsockPacketOp::Request)
            .set_buf_alloc(65536)
            .set_fwd_cnt(0);

        let d_tx = harness.add_tx_readable(hdr_as_bytes(&req_hdr));
        harness.publish_tx(d_tx);
        notify.queue_notify(VSOCK_TX_QUEUE).unwrap();

        // Accept the connection, wait for RESPONSE
        let accepted = listener.accept().unwrap().0;
        wait_for_condition(|| harness.rx_used_idx() >= 1, 5000);

        // Close the host-side socket to produce EOF
        drop(accepted);

        // The poller should detect EOF on the next POLLIN and send
        // a SHUTDOWN packet to the guest.
        wait_for_condition(|| harness.rx_used_idx() >= 2, 5000);

        // Read back the packet from RX used ring entry 1
        let (hdr, _data) = harness.read_vsock_packet(1);

        assert_eq!(hdr.op(), Some(VsockPacketOp::Shutdown));
        assert_eq!(hdr.src_cid(), VSOCK_HOST_CID);
        assert_eq!(hdr.dst_cid(), guest_cid as u64);
        assert_eq!(hdr.src_port(), vsock_port);
        assert_eq!(hdr.dst_port(), guest_port);
        assert_eq!(
            hdr.flags(),
            VsockPacketFlags::VIRTIO_VSOCK_SHUTDOWN_F_SEND
                | VsockPacketFlags::VIRTIO_VSOCK_SHUTDOWN_F_RECEIVE
        );

        notify.shutdown().unwrap();
        handle.join().unwrap();
    }
}
