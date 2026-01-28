use iddqd::IdHashMap;
use lazy_static::lazy_static;
use slog::Logger;
use std::net::SocketAddr;
use std::sync::Arc;

use crate::accessors::MemAccessor;
use crate::common::*;
use crate::hw::pci;
use crate::hw::virtio;
use crate::hw::virtio::queue::Chain;
use crate::hw::virtio::queue::VirtQueue;
use crate::hw::virtio::queue::VqSize;
use crate::migrate::*;
use crate::util::regmap::RegMap;
use crate::vmm::MemCtx;
use crate::vsock::packet::VsockPacket;
use crate::vsock::packet::VsockPacketError;
use crate::vsock::packet::VsockPacketHeader;
use crate::vsock::proxy::BackendListener;
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
/// The permit holds a mutable reference to `VsockVq`, ensuring only one permit
/// can exist at a time (enforced at compile time). If dropped without calling
/// `write_rw`, the chain is retained in `VsockVq` for reuse.
pub struct RxPermit<'a> {
    available_data_space: usize,
    vq: &'a mut VsockVq,
}

impl RxPermit<'_> {
    /// Returns the maximum data payload that can fit in this descriptor chain.
    pub fn available_data_space(&self) -> usize {
        self.available_data_space
    }

    pub fn write(self, header: &VsockPacketHeader, data: &[u8]) {
        let mem = self.vq.acc_mem.access().expect("mem access for write");
        let queue =
            self.vq.queues.get(VSOCK_RX_QUEUE as usize).expect("rx queue");
        let mut chain = self.vq.rx_chain.take().expect("rx_chain should exist");

        chain.write(header, &mem);

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

        queue.push_used(&mut chain, &mem);
    }
}

pub struct VsockVq {
    queues: Vec<Arc<VirtQueue>>,
    acc_mem: MemAccessor,
    /// Cached rx chain for permit reuse when dropped without write_rw
    rx_chain: Option<Chain>,
}

impl VsockVq {
    pub(crate) fn new(
        queues: Vec<Arc<VirtQueue>>,
        acc_mem: MemAccessor,
    ) -> Self {
        Self { queues, acc_mem, rx_chain: None }
    }

    /// Try to acquire a permit for sending a packet to the guest.
    ///
    /// Returns `Some(RxPermit)` if a descriptor chain is available,
    /// `None` if the rx queue is full.
    pub fn try_rx_permit(&mut self) -> Option<RxPermit<'_>> {
        // Reuse cached chain or pop a new one
        if self.rx_chain.is_none() {
            let mem = self.acc_mem.access()?;
            let vq = self.queues.get(VSOCK_RX_QUEUE as usize)?;
            let mut chain = Chain::with_capacity(10);
            vq.pop_avail(&mut chain, &mem)?;
            self.rx_chain = Some(chain);
        }

        let header_size = std::mem::size_of::<VsockPacketHeader>();
        let available_data_space = self
            .rx_chain
            .as_ref()
            .unwrap()
            .remain_write_bytes()
            .saturating_sub(header_size);

        Some(RxPermit { available_data_space, vq: self })
    }

    /// Receive all available packets from the TX queue.
    ///
    /// Returns a Vec of parsed packets. In the future this may be refactored
    /// to return an iterator over GuestRegions to avoid copying packet data.
    pub fn recv_packet(&self) -> Option<Result<VsockPacket, VsockPacketError>> {
        let mem = self.acc_mem.access()?;
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
}

pub struct PciVirtioSock {
    cid: u32,
    backend: VsockProxy,
    virtio_state: PciVirtioState,
    pci_state: pci::DeviceState,
}

impl PciVirtioSock {
    pub fn new(queue_size: u16, cid: u32, log: Logger) -> Arc<Self> {
        let queues = VirtQueues::new(&[
            // VSOCK_RX_QUEUE
            VqSize::new(queue_size),
            // VSOCK_TX_QUEUE
            VqSize::new(queue_size),
            // VSOCK_EVENT_QUEUE
            VqSize::new(1),
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
        let mut listeners = IdHashMap::new();
        listeners.insert_overwrite(BackendListener::new(
            3000,
            "127.0.0.1:3000".parse().unwrap(),
        ));

        let backend = VsockProxy::new(cid, vvq, log, listeners);

        Arc::new(Self { cid, backend, virtio_state, pci_state })
    }

    // fn _send_transport_reset(&self) {
    //     let vq = &self.virtio_state.queues.get(VSOCK_EVENT_QUEUE).unwrap();
    //     let mem = vq.acc_mem.access().unwrap();
    //     let mut chain = Chain::with_capacity(1);

    //     // Pop a buffer from the event queue
    //     if let Some((_idx, _clen)) = vq.pop_avail(&mut chain, &mem) {
    //         // Write the transport reset event
    //         let event =
    //             VirtioVsockEvent { id: VIRTIO_VSOCK_EVENT_TRANSPORT_RESET };
    //         chain.write(&event, &mem);

    //         // Push to used ring (this will also send interrupt to guest)
    //         vq.push_used(&mut chain, &mem);
    //     } else {
    //         eprintln!("no event queue buffer available for transport reset");
    //     }
    // }
}

impl VirtioDevice for PciVirtioSock {
    fn rw_dev_config(&self, mut rwo: crate::common::RWOp) {
        VSOCK_DEV_REGS.process(&mut rwo, |id, rwo| match rwo {
            RWOp::Read(ro) => match id {
                VsockReg::GuestCid => {
                    ro.write_u32(self.cid);
                    // The upper 32 bits are reserved and zeroed.
                    ro.fill(0);
                }
            },
            RWOp::Write(_) => {}
        })
    }

    fn features(&self) -> u64 {
        VIRTIO_VSOCK_F_STREAM
    }

    fn set_features(&self, _feat: u64) -> Result<(), ()> {
        Ok(())
    }

    fn mode(&self) -> virtio::Mode {
        virtio::Mode::Transitional
    }

    fn queue_notify(&self, vq: &VirtQueue) {
        let _ = self.backend.queue_notify(vq.id);
    }
}

// #[repr(C, packed)]
// #[derive(Copy, Clone, Default, Debug)]
// struct VirtioVsockEvent {
//     id: u32,
// }

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
        "pci-virtio-vsock"
    }
    fn reset(&self) {
        self.virtio_state.reset(self);
    }
    fn migrate(&'_ self) -> Migrator<'_> {
        Migrator::NonMigratable
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
    #![allow(unused)]
    pub const VIRTIO_VSOCK_CFG_SIZE: usize = 0x8;

    pub const VIRTIO_VSOCK_F_STREAM: u64 = 0;

    pub const VIRTIO_VSOCK_EVENT_TRANSPORT_RESET: u32 = 0;

    // Virtio vsock packet header is 44 bytes
    pub const VSOCK_PKT_HEADER_SZ: u32 = 0x2c;
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

        let len = usize::try_from(packet.header.len())
            .expect("running on a 64bit platform");
        packet.data.resize(len, 0);

        let mut done = 0;
        let copied = chain.for_remaining_type(true, |addr, len| {
            let mut remain = GuestData::from(&mut packet.data[done..]);
            if let Some(copied) = mem.read_into(addr, &mut remain, len) {
                let need_more = copied != remain.len();
                done += copied;
                (copied, need_more)
            } else {
                (0, false)
            }
        });

        if copied != len {
            return Err(VsockPacketError::InsufficientBytes {
                expected: len,
                remaining: copied,
            });
        }

        Ok(packet)
    }
}
