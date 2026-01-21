use lazy_static::lazy_static;
use slog::Logger;
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

    /// Write an RW packet directly into the descriptor chain.
    ///
    /// Consumes the permit and pushes to the used ring.
    pub fn write_rw(
        self,
        guest_cid: u32,
        src_port: u32,
        dst_port: u32,
        buf_alloc: u32,
        fwd_cnt: u32,
        data: &[u8],
    ) {
        let mem = self.vq.acc_mem.access().expect("mem access for write_rw");
        let queue =
            self.vq.queues.get(VSOCK_RX_QUEUE as usize).expect("rx queue");
        let mut chain = self.vq.rx_chain.take().expect("rx_chain should exist");

        let mut header = VsockPacketHeader::default();
        header
            .set_src_cid(crate::vsock::VSOCK_HOST_CID as u32)
            .set_dst_cid(guest_cid)
            .set_src_port(src_port)
            .set_dst_port(dst_port)
            .set_len(data.len() as u32)
            .set_socket_type(crate::vsock::packet::VIRTIO_VSOCK_TYPE_STREAM)
            .set_op(crate::vsock::packet::VIRTIO_VSOCK_OP_RW)
            .set_buf_alloc(buf_alloc)
            .set_fwd_cnt(fwd_cnt);

        chain.write(&header, &mem);

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
    fn new(queues: Vec<Arc<VirtQueue>>, acc_mem: MemAccessor) -> Self {
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

    /// Check if the rx queue has available descriptors for sending to guest.
    pub fn rx_queue_has_space(&self) -> bool {
        let mem = self.acc_mem.access().unwrap();
        let vq = self.queues.get(VSOCK_RX_QUEUE as usize).unwrap();
        !vq.avail_is_empty(&mem)
    }

    pub fn send_packet(&self, packet: &VsockPacket) -> Option<usize> {
        let mem = self.acc_mem.access()?;
        let vq = self
            .queues
            .get(VSOCK_RX_QUEUE as usize)
            .expect("vsock has rx queue");

        let VsockPacket { header, data } = packet;
        let mut data_offset = 0;
        let mut total_sent = 0;

        // Loop to send data across multiple descriptor chains if needed
        while data_offset < data.len() || data_offset == 0 {
            let mut chain = Chain::with_capacity(10);
            let Some((_idx, _clen)) = vq.pop_avail(&mut chain, &mem) else {
                eprintln!("send_packet: no descriptor available");
                if total_sent > 0 {
                    // Partial send - return what we sent
                    return Some(total_sent);
                }
                return None;
            };

            // Calculate how much data we can fit in this descriptor
            // First check available space, subtract header size
            let header_size = std::mem::size_of::<VsockPacketHeader>();
            let available_for_data =
                chain.remain_write_bytes().saturating_sub(header_size);
            let remaining_data = &data[data_offset..];
            let chunk_len =
                std::cmp::min(remaining_data.len(), available_for_data);

            // Create header for this chunk with correct length
            let mut chunk_header = *header;
            chunk_header.set_len(chunk_len as u32);

            chain.write(&chunk_header, &mem);

            let mut done = 0;
            chain.for_remaining_type(false, |addr, len| {
                let to_write = &remaining_data[done..chunk_len];
                if let Some(copied) = mem.write_from(addr, to_write, len) {
                    let need_more = copied != to_write.len();
                    done += copied;
                    (copied, need_more)
                } else {
                    (0, false)
                }
            });

            vq.push_used(&mut chain, &mem);

            data_offset += done;
            total_sent += done;

            // If we couldn't write any data in this iteration (and we had data), stop
            if done == 0 && !remaining_data.is_empty() {
                break;
            }

            // If no data to send (e.g., RST packet), exit after one iteration
            if data.is_empty() {
                break;
            }
        }

        Some(total_sent)
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
        let backend = VsockProxy::new(cid, vvq, log);

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
        self.backend.queue_notify(vq.id);
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
