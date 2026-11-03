// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use lazy_static::lazy_static;
use slog::Logger;
use std::sync::Arc;

use crate::accessors::Guard;
use crate::accessors::MemAccessor;
use crate::common::*;
use crate::hw::pci;
use crate::hw::virtio;
use crate::hw::virtio::queue::AvailChain;
use crate::hw::virtio::queue::Chain;
use crate::hw::virtio::queue::VirtQueue;
use crate::hw::virtio::queue::VqSize;
use crate::migrate::*;
use crate::util::regmap::RegMap;
use crate::vmm::mem::MemAccessed;
use crate::vmm::MemCtx;
use crate::vsock::packet::VsockPacket;
use crate::vsock::packet::VsockPacketError;
use crate::vsock::packet::VsockPacketHeader;
use crate::vsock::probes;
use crate::vsock::proxy::VsockPortMapping;
use crate::vsock::GuestCid;
use crate::vsock::VsockBackend;
use crate::vsock::VsockProxy;

use super::pci::PciVirtio;
use super::pci::PciVirtioState;
use super::queue::VirtQueues;
use super::VirtioDevice;

// virtio queue index numbers for virtio socket devices
pub const VSOCK_RX_QUEUE: u16 = 0x0;
pub const VSOCK_TX_QUEUE: u16 = 0x1;
pub const VSOCK_EVENT_QUEUE: u16 = 0x2;

/// A permit representing a reserved rx queue descriptor chain.
///
/// This guarantees we have space to send a packet to the guest before reading
/// data from a host socket, preventing data loss if the queue is full.
///
/// NB: The underlying descriptor chain is not consumed until a call to
/// [`RxPermit::write`] is made. Droopping the [`RxPermit`] is a no-op without a
/// call to write is a no-op.
pub struct RxPermit<'a> {
    avail: AvailChain<'a>,
    mem: Guard<'a, MemAccessed>,
}

impl RxPermit<'_> {
    /// Returns the maximum data payload that can fit in this descriptor chain.
    pub fn available_data_space(&self) -> usize {
        let header_size = std::mem::size_of::<VsockPacketHeader>();
        self.avail.chain().remain_write_bytes().saturating_sub(header_size)
    }

    pub fn write(mut self, header: &VsockPacketHeader, data: &[u8]) {
        let mem = &*self.mem;
        let chain = self.avail.chain_mut();
        chain.write(header, mem);

        if !data.is_empty() {
            let mut done = 0;
            chain.for_remaining_type(false, |addr, len| {
                let to_write = &data[done..];
                if let Some(copied) = mem.write_from(addr, to_write, len) {
                    let need_more = copied != to_write.len();
                    done += copied;
                    (copied, need_more)
                } else {
                    (0, false)
                }
            });
        }

        probes::vsock_pkt_rx!(|| header);
        self.avail.push_used(mem);
    }
}

#[derive(Debug, thiserror::Error)]
pub enum TransportResetError {
    #[error("virt queue not alive")]
    NotAlive,
    #[error("mem access failed")]
    MemAccess,
    #[error("event queue has no usable descriptors")]
    NoDescriptor,
    #[error("descriptor chain buffer too small")]
    ChainTooSmall,
}

pub struct VsockVq {
    queues: Vec<Arc<VirtQueue>>,
    acc_mem: MemAccessor,
}

impl VsockVq {
    pub(crate) fn new(
        queues: Vec<Arc<VirtQueue>>,
        acc_mem: MemAccessor,
    ) -> Self {
        Self { queues, acc_mem }
    }

    /// Try to acquire a permit for sending a packet to the guest.
    ///
    /// Returns `Some(RxPermit)` if a descriptor chain is available, `None`
    /// if the rx queue is full.
    pub fn try_rx_permit(&mut self) -> Option<RxPermit<'_>> {
        let vq = self.queues.get(VSOCK_RX_QUEUE as usize)?;
        // See propolis#1110 & propolis#1115
        // If propolis-server has started the vsock device but a different
        // device has encountered an error at startup there's a good chance
        // we attempt to access guest memory and panic. A way of preventing
        // us from doing that is to first check if the virtqueue is alive. A
        // virtqueue only becomes alive once a guest vCPU has ran.
        if !vq.is_alive() {
            return None;
        }

        let mem = self.acc_mem.access()?;
        let avail = vq.peek_avail(&mem)?;
        Some(RxPermit { avail, mem })
    }

    /// Receive all available packets from the TX queue.
    ///
    /// Returns a Vec of parsed packets. In the future this may be refactored
    /// to return an iterator over GuestRegions to avoid copying packet data.
    pub fn recv_packet(&self) -> Option<Result<VsockPacket, VsockPacketError>> {
        // TODO: cannot access memory?
        let mem = self.acc_mem.access().expect("mem access for read");
        let vq = self
            .queues
            .get(VSOCK_TX_QUEUE as usize)
            .expect("vsock has tx queue");

        let mut chain = Chain::with_capacity(10);
        let Some((_idx, _clen)) = vq.pop_avail(&mut chain, &mem) else {
            return None;
        };

        let packet = VsockPacket::parse(&mut chain, &mem);
        vq.push_used(&mut chain, &mem);

        Some(packet)
    }

    /// Send a `VIRTIO_VSOCK_EVENT_TRANSPORT_RESET` to the guest on the
    /// event queue.
    ///
    /// virtio spec 5.10.6.7, states that this must be sent upon guest
    /// migration, so that the guest can remove all of its established
    /// connections.
    pub fn send_transport_reset(&mut self) -> Result<(), TransportResetError> {
        let vq = self
            .queues
            .get(VSOCK_EVENT_QUEUE as usize)
            .expect("has event queue");

        if !vq.is_alive() {
            return Err(TransportResetError::NotAlive);
        }

        let mem =
            self.acc_mem.access().ok_or(TransportResetError::MemAccess)?;
        let mut avail =
            vq.peek_avail(&mem).ok_or(TransportResetError::NoDescriptor)?;

        let event_id: u32 = VIRTIO_VSOCK_EVENT_TRANSPORT_RESET.to_le();
        if !avail.chain_mut().write(&event_id, &mem) {
            avail.push_used(&mem);
            return Err(TransportResetError::ChainTooSmall);
        }

        avail.push_used(&mem);
        Ok(())
    }
}

pub struct PciVirtioSock {
    cid: GuestCid,
    backend: VsockProxy,
    virtio_state: PciVirtioState,
    pci_state: pci::DeviceState,
}

impl PciVirtioSock {
    pub fn new(
        queue_size: u16,
        cid: GuestCid,
        log: Logger,
        port_mappings: Vec<VsockPortMapping>,
    ) -> Arc<Self> {
        let queues = VirtQueues::new(&[
            // VSOCK_RX_QUEUE
            VqSize::new(queue_size),
            // VSOCK_TX_QUEUE
            VqSize::new(queue_size),
            // VSOCK_EVENT_QUEUE
            VqSize::new(16),
        ]);

        // One for rx, tx, event
        let msix_count = Some(3);
        let (virtio_state, pci_state) = PciVirtioState::new(
            virtio::Mode::Transitional,
            queues,
            msix_count,
            virtio::DeviceId::Socket,
            VIRTIO_VSOCK_CFG_SIZE,
        );

        let vvq = VsockVq::new(
            virtio_state.queues.iter().map(Clone::clone).collect(),
            pci_state.acc_mem.child(Some("vsock rx queue".to_string())),
        );
        let port_mappings = port_mappings.into_iter().collect();

        let backend = VsockProxy::new(log, cid, vvq, port_mappings);

        Arc::new(Self { cid, backend, virtio_state, pci_state })
    }
}

impl VirtioDevice for PciVirtioSock {
    fn rw_dev_config(&self, mut rwo: crate::common::RWOp) {
        VSOCK_DEV_REGS.process(&mut rwo, |id, rwo| match rwo {
            RWOp::Read(ro) => match id {
                VsockReg::GuestCid => {
                    ro.write_u64(self.cid.get());
                }
            },
            RWOp::Write(_) => {}
        })
    }

    fn features(&self) -> u64 {
        // We support VIRTIO_VSOCK_F_STREAM
        //
        // virtio spec 1.3:
        // The device SHOULD offer the VIRTIO_VSOCK_F_NO_IMPLIED_STREAM feature.
        (VsockFeatures::NO_IMPLIED_STREAM | VsockFeatures::STREAM).bits()
    }

    fn set_features(&self, feat: u64) -> Result<(), ()> {
        // We only care about the vsock specific bits so grab just those
        match VsockFeatures::from_bits_truncate(feat) {
            // If no feature bit has been negotiated, the device SHOULD act as
            // if VIRTIO_VSOCK_F_STREAM has been negotiated.
            f if f.is_empty() => Ok(()),
            f if f == VsockFeatures::STREAM => Ok(()),
            // We have not advertised SEQPACKET so we don't expect it to show up
            // here.
            _ => Err(()),
        }
    }

    fn mode(&self) -> virtio::Mode {
        virtio::Mode::Transitional
    }

    fn queue_notify(&self, vq: &VirtQueue) {
        let _ = self.backend.queue_notify(vq.id);
    }
}

impl PciVirtio for PciVirtioSock {
    fn virtio_state(&self) -> &PciVirtioState {
        &self.virtio_state
    }
    fn pci_state(&self) -> &pci::DeviceState {
        &self.pci_state
    }
}

impl Lifecycle for PciVirtioSock {
    fn type_name(&self) -> &'static str {
        "pci-virtio-socket"
    }
    fn start(&self) -> Result<(), anyhow::Error> {
        self.backend.start();
        Ok(())
    }
    fn pause(&self) {
        let _ = self.backend.pause();
        self.backend.wait_stopped();
    }
    fn reset(&self) {
        self.virtio_state.reset(self);
        self.backend.reset();
    }
    fn resume(&self) {
        self.backend.resume();
    }
    fn halt(&self) {
        self.backend.halt();
    }
    fn migrate(&'_ self) -> Migrator<'_> {
        Migrator::Multi(self)
    }
}

impl MigrateMulti for PciVirtioSock {
    fn export(
        &self,
        output: &mut PayloadOutputs,
        ctx: &MigrateCtx,
    ) -> Result<(), MigrateStateError> {
        <dyn PciVirtio>::export(self, output, ctx)
    }

    fn import(
        &self,
        offer: &mut PayloadOffers,
        ctx: &MigrateCtx,
    ) -> Result<(), MigrateStateError> {
        <dyn PciVirtio>::import(self, offer, ctx)?;

        // Upon import, our vsock poller has no state and the spec indicates
        // that we need to tell the guest to reset its established connections.
        self.backend.send_transport_reset();

        Ok(())
    }
}

#[derive(Copy, Clone, Eq, PartialEq, Debug)]
enum VsockReg {
    GuestCid,
}

lazy_static! {
    static ref VSOCK_DEV_REGS: RegMap<VsockReg> = {
        let layout = [(VsockReg::GuestCid, 8)];
        RegMap::create_packed(VIRTIO_VSOCK_CFG_SIZE, &layout, None)
    };
}

mod bits {
    pub const VIRTIO_VSOCK_CFG_SIZE: usize = 0x8;

    bitflags! {
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub struct VsockFeatures: u64 {
            const STREAM    = 1 << 0;
            const SEQPACKET = 1 << 1;
            const NO_IMPLIED_STREAM = 1 << 2;
        }
    }

    pub const VIRTIO_VSOCK_EVENT_TRANSPORT_RESET: u32 = 0;
}
use bits::*;

impl VsockPacket {
    // TODO: We may want to consider operating on `Vec<GuestRegion>` to avoid
    // double copying the packet contents. For now we are reading all of the
    // packet data at once because it's convenient.
    fn parse(
        chain: &mut Chain,
        mem: &MemCtx,
    ) -> Result<Self, VsockPacketError> {
        let mut packet = VsockPacket::default();

        // Attempt to read the vsock packet header from the descriptor chain
        // before we can process the full packet.
        if !chain.read(&mut packet.header, mem) {
            return Err(VsockPacketError::ChainHeaderRead);
        }

        // If the packet header indicates there is no data in this packet, then
        // there's no point in attempting to continue reading from the chain.
        if packet.header.len() == 0 {
            return Ok(packet);
        }

        let hdr_len = usize::try_from(packet.header.len())
            .expect("running on a 64bit platform");
        let chain_len = chain.remain_read_bytes();

        // Ensure that the vsock packet header length matches the reality of
        // the desc chain.
        if hdr_len > chain_len {
            return Err(VsockPacketError::InvalidPacketLen {
                hdr_len,
                chain_len,
            });
        }
        let mut data = vec![0; hdr_len];

        // While we are here we should validate that packets cid fields do no
        // contain reserved bits
        if packet.header.src_cid() >> 32 != 0 {
            return Err(VsockPacketError::InvalidSrcCid {
                src_cid: packet.header.src_cid(),
            });
        }
        if packet.header.dst_cid() >> 32 != 0 {
            return Err(VsockPacketError::InvalidDstCid {
                dst_cid: packet.header.dst_cid(),
            });
        }

        let mut done = 0;
        let copied = chain.for_remaining_type(true, |addr, len| {
            let mut remain = GuestData::from(&mut data[done..]);
            if let Some(copied) = mem.read_into(addr, &mut remain, len) {
                let need_more = copied != remain.len();
                done += copied;
                (copied, need_more)
            } else {
                (0, false)
            }
        });

        // If we fail to copy the correct amount of bytes from the desc chain
        // something is clearly wrong.
        if copied != hdr_len {
            return Err(VsockPacketError::InsufficientBytes {
                expected: hdr_len,
                remaining: copied,
            });
        }

        packet.data = data.into();

        Ok(packet)
    }
}

#[cfg(test)]
mod test {
    #[cfg(target_os = "illumos")]
    mod illumos {
        use std::net::TcpListener;
        use std::sync::atomic::Ordering;
        use std::sync::Arc;
        use std::time::{Duration, Instant};

        use crate::hw::pci::test::Scaffold;
        use crate::hw::pci::{BusLocation, Endpoint};
        use crate::hw::virtio::testutil::{TestVirtQueues, VqSize};
        use crate::hw::virtio::vsock::{
            PciVirtioSock, VIRTIO_VSOCK_EVENT_TRANSPORT_RESET,
            VSOCK_EVENT_QUEUE, VSOCK_RX_QUEUE, VSOCK_TX_QUEUE,
        };
        use crate::hw::virtio::VirtioDevice;
        use crate::lifecycle::Lifecycle;
        use crate::migrate::{
            MigrateCtx, MigrateMulti, PayloadOffer, PayloadOffers,
            PayloadOutputs,
        };
        use crate::vsock::packet::{
            VsockPacketHeader, VsockPacketOp, VsockSocketType,
        };
        use crate::vsock::proxy::VsockPortMapping;
        use crate::vsock::{GuestCid, VSOCK_HOST_CID};
        use zerocopy::{FromBytes, IntoBytes};

        fn test_logger() -> slog::Logger {
            slog::Logger::root(slog::Discard, slog::o!())
        }

        fn test_deadline() -> Instant {
            Instant::now() + Duration::from_secs(15)
        }

        #[test]
        fn migrate_multi_round_trip() {
            const SRC_PORT: u32 = 1234;
            const DST_PORT: u32 = 3000;
            let cid = GuestCid::try_from(716).unwrap();

            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let mappings = vec![VsockPortMapping::new(
                3000,
                listener.local_addr().unwrap(),
            )];

            let source =
                PciVirtioSock::new(256, cid, test_logger(), mappings.clone());
            let target = PciVirtioSock::new(256, cid, test_logger(), mappings);

            let tvqs = TestVirtQueues::new(&[
                VqSize::new(256),
                VqSize::new(256),
                VqSize::new(16),
            ]);

            // Scaffold sets up an orphan acc_mem, swap it for a child of the
            // test memory's accessor tree.
            let mut scaffold = Scaffold::new();
            scaffold.acc_mem = tvqs.mem_acc().child(Some("pci-bus".into()));
            let bus = scaffold.create_bus();

            // Source vsock device
            bus.attach(
                BusLocation::new(0, 0).unwrap(),
                Arc::clone(&source) as Arc<dyn Endpoint>,
                None,
            );
            // Dest vsock device
            bus.attach(
                BusLocation::new(1, 0).unwrap(),
                Arc::clone(&target) as Arc<dyn Endpoint>,
                None,
            );

            // Setup the test vsock queues, but don't provide the event buffer.
            for qid in [VSOCK_RX_QUEUE, VSOCK_EVENT_QUEUE, VSOCK_TX_QUEUE] {
                let layout = tvqs.layout(qid as usize);
                let vq = source
                    .virtio_state
                    .queues
                    .get(qid)
                    .expect("device has the queue");
                vq.map_virtqueue(
                    layout.desc_base,
                    layout.avail_base,
                    layout.used_base,
                );
                vq.live.store(true, Ordering::Release);
                vq.enabled.store(true, Ordering::Release);
            }
            let mut ev_writer = tvqs.writer(VSOCK_EVENT_QUEUE as usize, 0);
            let mut tx_writer = tvqs.writer(VSOCK_TX_QUEUE as usize, 0x10000);
            let mut rx_writer = tvqs.writer(VSOCK_RX_QUEUE as usize, 0x20000);

            let d_rx = rx_writer.add_writable(tvqs.mem_acc(), 256);
            rx_writer.publish_avail(tvqs.mem_acc(), d_rx);

            let acc_mem = tvqs.mem_acc().access().expect("test memory");
            let ctx = MigrateCtx { mem: &acc_mem };

            // Export guest state
            let mut outputs = PayloadOutputs::new();
            MigrateMulti::export(source.as_ref(), &mut outputs, &ctx)
                .expect("can export PciVirtioSock");

            // Serialize the export payload into the import payload.
            let mut offers =
                PayloadOffers::new(outputs.into_iter().map(|output| {
                    let value = serde_json::to_value(&output.payload)
                        .expect("serializing payload output");
                    PayloadOffer {
                        kind: output.kind,
                        version: output.version,
                        payload: Box::new(
                            <dyn erased_serde::Deserializer>::erase(value),
                        ),
                    }
                }));

            // Import guest state
            MigrateMulti::import(target.as_ref(), &mut offers, &ctx)
                .expect("can import PciVirtioSock");

            // After import, the guest would be resumed
            Lifecycle::resume(target.as_ref());

            // Attempt to open a connection via REQUEST, which should not
            // succeed yet, because our event queue has no buffers in it yet.
            let mut hdr = VsockPacketHeader::new();
            hdr.set_src_cid(cid)
                .set_dst_cid_raw(VSOCK_HOST_CID)
                .set_src_port(SRC_PORT)
                .set_dst_port(DST_PORT)
                .set_len(0)
                .set_socket_type(VsockSocketType::Stream)
                .set_op(VsockPacketOp::Request)
                .set_buf_alloc(65536)
                .set_fwd_cnt(0);
            let d_tx = tx_writer.add_readable(tvqs.mem_acc(), hdr.as_bytes());
            tx_writer.publish_avail(tvqs.mem_acc(), d_tx);
            let tx_vq = target
                .virtio_state
                .queues
                .get(VSOCK_TX_QUEUE)
                .expect("device has a tx queue");
            VirtioDevice::queue_notify(target.as_ref(), tx_vq);

            std::thread::sleep(Duration::from_millis(100));
            assert_eq!(
                tx_writer.used_idx(tvqs.mem_acc()),
                0,
                "tx packet processed while the transport reset was needed"
            );

            // Pretend the guest updates its event queue with new buffers. A
            // well behaved guest should have kept these available from the
            // start.
            let d_ev = ev_writer.add_writable(tvqs.mem_acc(), 4);
            ev_writer.publish_avail(tvqs.mem_acc(), d_ev);
            let ev_vq = target
                .virtio_state
                .queues
                .get(VSOCK_EVENT_QUEUE)
                .expect("device has an event queue");
            VirtioDevice::queue_notify(target.as_ref(), ev_vq);

            let deadline = test_deadline();
            while ev_writer.used_idx(tvqs.mem_acc()) < 1 {
                assert!(
                    Instant::now() < deadline,
                    "timed out waiting for transport reset delivery"
                );
                std::thread::sleep(Duration::from_millis(10));
            }

            let elem = ev_writer.read_used_elem(tvqs.mem_acc(), 0);
            assert_eq!(elem.id, u32::from(d_ev));
            assert_eq!(elem.len, 4, "event is a single le32 id");
            let event = ev_writer.read_desc_data(tvqs.mem_acc(), d_ev, 4);
            assert_eq!(
                event,
                VIRTIO_VSOCK_EVENT_TRANSPORT_RESET.to_le_bytes(),
                "delivered event is VIRTIO_VSOCK_EVENT_TRANSPORT_RESET"
            );

            // Once we were able to send a VIRTIO_VSOCK_EVENT_TRANSPORT_RESET to
            // the guest we should now see the REQUEST acknowledged.
            let deadline = test_deadline();
            while tx_writer.used_idx(tvqs.mem_acc()) < 1 {
                assert!(
                    Instant::now() < deadline,
                    "timed out waiting for the gated tx packet"
                );
                std::thread::sleep(Duration::from_millis(10));
            }
            let tx_elem = tx_writer.read_used_elem(tvqs.mem_acc(), 0);
            assert_eq!(tx_elem.id, u32::from(d_tx));

            let deadline = Instant::now() + Duration::from_secs(15);
            while rx_writer.used_idx(tvqs.mem_acc()) < 1 {
                assert!(
                    Instant::now() < deadline,
                    "timed out waiting for the handshake RESPONSE"
                );
                std::thread::sleep(Duration::from_millis(10));
            }
            let rx_elem = rx_writer.read_used_elem(tvqs.mem_acc(), 0);
            assert_eq!(rx_elem.id, u32::from(d_rx));
            let buf = rx_writer.read_desc_data(
                tvqs.mem_acc(),
                d_rx,
                rx_elem.len as usize,
            );
            let resp_hdr = VsockPacketHeader::read_from_bytes(
                &buf[..std::mem::size_of::<VsockPacketHeader>()],
            )
            .expect("rx buffer holds a packet header");
            assert_eq!(resp_hdr.op(), Some(VsockPacketOp::Response));
            assert_eq!(resp_hdr.src_cid(), VSOCK_HOST_CID);
            assert_eq!(resp_hdr.dst_cid(), cid.get());
            assert_eq!(resp_hdr.src_port(), DST_PORT);
            assert_eq!(resp_hdr.dst_port(), SRC_PORT);

            // Tear down the source and target vsock devices.
            Lifecycle::pause(target.as_ref());
            Lifecycle::halt(source.as_ref());
            Lifecycle::halt(target.as_ref());
        }
    }
}
