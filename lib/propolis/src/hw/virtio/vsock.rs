// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

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
use crate::vsock::proxy::VsockPortMapping;
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
/// `write`, the chain is retained in `VsockVq` for reuse.
pub struct RxPermit<'a> {
    vq: &'a mut VsockVq,
}

impl RxPermit<'_> {
    /// Returns the maximum data payload that can fit in this descriptor chain.
    pub fn available_data_space(&self) -> usize {
        let header_size = std::mem::size_of::<VsockPacketHeader>();
        self.vq
            .rx_chain
            .as_ref()
            .expect("has chain")
            .remain_write_bytes()
            .saturating_sub(header_size)
    }

    pub fn write(self, header: &VsockPacketHeader, data: &[u8]) {
        // TODO: cannot access memory?
        let mem = self.vq.acc_mem.access().expect("mem access for write");
        let queue =
            self.vq.queues.get(VSOCK_RX_QUEUE as usize).expect("rx queue");

        // SAFETY: `RxPermit` should only be created if the owning `VsockVq`
        // actually has a `Some(Chain)`. Unfortuantely there doesn't seem to be
        // a way to enforce this at compile time.
        let mut chain = self.vq.rx_chain.take().expect("has chain");
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
    /// Cached rx chain for permit reuse when dropped without write
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
            // TODO: cannot access memory?
            let mem = self.acc_mem.access().expect("mem access for write");
            let vq = self.queues.get(VSOCK_RX_QUEUE as usize)?;
            let mut chain = Chain::with_capacity(10);
            if let Some(_) = vq.pop_avail(&mut chain, &mem) {
                self.rx_chain = Some(chain);
            }
        }

        // We only return a permit iff we know that we are holding onto a valid
        // descriptor chain that can be used by the borrowing `RxPermit`
        match self.rx_chain {
            Some(_) => Some(RxPermit { vq: self }),
            None => None,
        }
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
}

pub struct PciVirtioSock {
    cid: u32,
    backend: VsockProxy,
    virtio_state: PciVirtioState,
    pci_state: pci::DeviceState,
}

impl PciVirtioSock {
    pub fn new(
        queue_size: u16,
        cid: u32,
        log: Logger,
        port_mappings: Vec<VsockPortMapping>,
    ) -> Arc<Self> {
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
        let port_mappings = port_mappings.into_iter().collect();

        let backend = VsockProxy::new(cid, vvq, log, port_mappings);

        Arc::new(Self { cid, backend, virtio_state, pci_state })
    }
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
    pub const VIRTIO_VSOCK_CFG_SIZE: usize = 0x8;

    pub const VIRTIO_VSOCK_F_STREAM: u64 = 1 << 0;

    #[allow(unused)]
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
