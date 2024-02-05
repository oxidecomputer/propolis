// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::num::NonZeroU16;
use std::sync::{Arc, Weak};

use crate::accessors::MemAccessor;
use crate::block;
use crate::common::*;
use crate::hw::pci;
use crate::migrate::*;
use crate::util::regmap::RegMap;

use super::bits::*;
use super::pci::{PciVirtio, PciVirtioState};
use super::queue::{Chain, VirtQueue, VirtQueues};
use super::VirtioDevice;
use bits::*;

use futures::future::BoxFuture;
use lazy_static::lazy_static;

/// Sizing for virtio-block is specified in 512B sectors
const SECTOR_SZ: usize = 512;

struct CompletionPayload {
    /// ID of original request.
    rid: u16,
    /// VirtIO chain in which we indicate the result.
    chain: Chain,
}

pub struct PciVirtioBlock {
    virtio_state: PciVirtioState,
    pci_state: pci::DeviceState,

    block_attach: block::device::Attachment,
    block_tracking: block::device::Tracking<CompletionPayload>,
}
impl PciVirtioBlock {
    pub fn new(queue_size: u16) -> Arc<Self> {
        let queues = VirtQueues::new(
            NonZeroU16::new(queue_size).unwrap(),
            NonZeroU16::new(1).unwrap(),
        );
        // virtio-block only needs two MSI-X entries for its interrupt needs:
        // - device config changes
        // - queue 0 notification
        let msix_count = Some(2);
        let (virtio_state, pci_state) = PciVirtioState::create(
            queues,
            msix_count,
            VIRTIO_DEV_BLOCK,
            VIRTIO_SUB_DEV_BLOCK,
            pci::bits::CLASS_STORAGE,
            VIRTIO_BLK_CFG_SIZE,
        );

        Arc::new_cyclic(|weak| Self {
            pci_state,
            virtio_state,
            block_attach: block::device::Attachment::new(),
            block_tracking: block::device::Tracking::new(
                weak.clone() as Weak<dyn block::Device>
            ),
        })
    }

    fn block_cfg_read(&self, id: &BlockReg, ro: &mut ReadOp) {
        let info = self.block_attach.info().unwrap_or_else(Default::default);

        let total_bytes = info.total_size * info.block_size as u64;
        match id {
            BlockReg::Capacity => {
                ro.write_u64(total_bytes / SECTOR_SZ as u64);
            }
            BlockReg::SegMax => {
                // XXX: Copy the static limit from qemu for now
                ro.write_u32(128 - 2);
            }
            BlockReg::BlockSize => ro.write_u32(info.block_size),
            BlockReg::Unused => {
                ro.fill(0);
            }
            _ => {
                // XXX: all zeroes for now
                ro.fill(0);
            }
        }
    }

    fn next_req(&self) -> Option<block::Request> {
        let vq = &self.virtio_state.queues[0];
        let mem = self.pci_state.acc_mem.access()?;

        let mut chain = Chain::with_capacity(4);
        // Pop a request off the queue if there's one available.
        // For debugging purposes, we'll also use the returned index
        // as a psuedo-id for the request to associate it with its
        // subsequent completion
        let (rid, _clen) = vq.pop_avail(&mut chain, &mem)?;

        let mut breq = VbReq::default();
        if !chain.read(&mut breq, &mem) {
            todo!("error handling");
        }
        let off = breq.sector as usize * SECTOR_SZ;
        let req = match breq.rtype {
            VIRTIO_BLK_T_IN => {
                // should be (blocksize * 512) + 1 remaining writable byte for status
                // TODO: actually enforce block size
                let blocks = (chain.remain_write_bytes() - 1) / SECTOR_SZ;
                let sz = blocks * SECTOR_SZ;

                if let Some(regions) = chain.writable_bufs(sz) {
                    probes::vioblk_read_enqueue!(|| (
                        rid, off as u64, sz as u64
                    ));
                    Ok(self.block_tracking.track(
                        block::Request::new_read(off, sz, regions),
                        CompletionPayload { rid, chain },
                    ))
                } else {
                    Err(chain)
                }
            }
            VIRTIO_BLK_T_OUT => {
                // should be (blocksize * 512) remaining read bytes
                let blocks = chain.remain_read_bytes() / SECTOR_SZ;
                let sz = blocks * SECTOR_SZ;

                if let Some(regions) = chain.readable_bufs(sz) {
                    probes::vioblk_write_enqueue!(|| (
                        rid, off as u64, sz as u64
                    ));
                    Ok(self.block_tracking.track(
                        block::Request::new_write(off, sz, regions),
                        CompletionPayload { rid, chain },
                    ))
                } else {
                    Err(chain)
                }
            }
            VIRTIO_BLK_T_FLUSH => {
                probes::vioblk_flush_enqueue!(|| (rid));
                Ok(self.block_tracking.track(
                    block::Request::new_flush(),
                    CompletionPayload { rid, chain },
                ))
            }
            _ => Err(chain),
        };
        match req {
            Err(mut chain) => {
                // try to set the status byte to failed
                let remain = chain.remain_write_bytes();
                if remain >= 1 {
                    chain.write_skip(remain - 1);
                    chain.write(&VIRTIO_BLK_S_UNSUPP, &mem);
                }
                vq.push_used(&mut chain, &mem);
                None
            }
            Ok(r) => Some(r),
        }
    }

    fn complete_req(
        &self,
        rid: u16,
        op: block::Operation,
        res: block::Result,
        chain: &mut Chain,
    ) {
        let vq = self.virtio_state.queues.get(0).expect("vq must exist");
        if let Some(mem) = vq.acc_mem.access() {
            let resnum = match res {
                block::Result::Success => VIRTIO_BLK_S_OK,
                block::Result::Failure => VIRTIO_BLK_S_IOERR,
                block::Result::ReadOnly => VIRTIO_BLK_S_IOERR,
                block::Result::Unsupported => VIRTIO_BLK_S_UNSUPP,
            };
            match op {
                block::Operation::Read(..) => {
                    probes::vioblk_read_complete!(|| (rid, resnum));
                }
                block::Operation::Write(..) => {
                    probes::vioblk_write_complete!(|| (rid, resnum));
                }
                block::Operation::Flush => {
                    probes::vioblk_flush_complete!(|| (rid, resnum));
                }
            }
            chain.write(&resnum, &mem);
            vq.push_used(chain, &mem);
        }
    }
}

impl VirtioDevice for PciVirtioBlock {
    fn cfg_rw(&self, mut rwo: RWOp) {
        BLOCK_DEV_REGS.process(&mut rwo, |id, rwo| match rwo {
            RWOp::Read(ro) => self.block_cfg_read(id, ro),
            RWOp::Write(_) => {
                //ignore writes
            }
        });
    }
    fn get_features(&self) -> u32 {
        let mut feat = VIRTIO_BLK_F_BLK_SIZE;
        feat |= VIRTIO_BLK_F_SEG_MAX;
        feat |= VIRTIO_BLK_F_FLUSH;

        let info = self.block_attach.info().unwrap_or_else(Default::default);
        if info.read_only {
            feat |= VIRTIO_BLK_F_RO;
        }
        feat
    }
    fn set_features(&self, _feat: u32) -> Result<(), ()> {
        // XXX: real features
        Ok(())
    }

    fn queue_notify(&self, _vq: &Arc<VirtQueue>) {
        self.block_attach.notify()
    }
}
impl PciVirtio for PciVirtioBlock {
    fn virtio_state(&self) -> &PciVirtioState {
        &self.virtio_state
    }
    fn pci_state(&self) -> &pci::DeviceState {
        &self.pci_state
    }
}
impl block::Device for PciVirtioBlock {
    fn attachment(&self) -> &block::device::Attachment {
        &self.block_attach
    }

    fn next(&self) -> Option<block::Request> {
        self.next_req()
    }

    fn complete(&self, res: block::Result, id: block::ReqId) {
        let (op, mut payload) = self.block_tracking.complete(id, res);
        let CompletionPayload { rid, ref mut chain } = payload;
        self.complete_req(rid, op, res, chain);
    }

    fn accessor_mem(&self) -> MemAccessor {
        self.pci_state.acc_mem.child(Some("block backend".to_string()))
    }
}
impl Lifecycle for PciVirtioBlock {
    fn type_name(&self) -> &'static str {
        "pci-virtio-block"
    }
    fn reset(&self) {
        self.virtio_state.reset(self);
    }
    fn pause(&self) {
        self.block_attach.pause();
    }
    fn resume(&self) {
        self.block_attach.resume();
    }
    fn paused(&self) -> BoxFuture<'static, ()> {
        Box::pin(self.block_tracking.none_outstanding())
    }
    fn migrate(&self) -> Migrator {
        Migrator::Multi(self)
    }
}
impl MigrateMulti for PciVirtioBlock {
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
        <dyn PciVirtio>::import(self, offer, ctx)
    }
}

#[derive(Copy, Clone, Debug, Default)]
#[repr(C)]
struct VbReq {
    rtype: u32,
    reserved: u32,
    sector: u64,
}

#[derive(Copy, Clone, Eq, PartialEq, Debug)]
enum BlockReg {
    Capacity,
    SizeMax,
    SegMax,
    GeoCyl,
    GeoHeads,
    GeoSectors,
    BlockSize,
    TopoPhysExp,
    TopoAlignOff,
    TopoMinIoSz,
    TopoOptIoSz,
    Writeback,
    Unused,
    MaxDiscardSectors,
    MaxDiscardSeg,
    DiscardSectorAlign,
    MaxZeroSectors,
    MaxZeroSeg,
    ZeroMayUnmap,
}
lazy_static! {
    static ref BLOCK_DEV_REGS: RegMap<BlockReg> = {
        let layout = [
            (BlockReg::Capacity, 8),
            (BlockReg::SizeMax, 4),
            (BlockReg::SegMax, 4),
            (BlockReg::GeoCyl, 2),
            (BlockReg::GeoHeads, 1),
            (BlockReg::GeoSectors, 1),
            (BlockReg::BlockSize, 4),
            (BlockReg::TopoPhysExp, 1),
            (BlockReg::TopoAlignOff, 1),
            (BlockReg::TopoMinIoSz, 2),
            (BlockReg::TopoOptIoSz, 4),
            (BlockReg::Writeback, 1),
            (BlockReg::Unused, 3),
            (BlockReg::MaxDiscardSectors, 4),
            (BlockReg::MaxDiscardSeg, 4),
            (BlockReg::DiscardSectorAlign, 4),
            (BlockReg::MaxZeroSectors, 4),
            (BlockReg::MaxZeroSeg, 4),
            (BlockReg::ZeroMayUnmap, 1),
            (BlockReg::Unused, 3),
        ];
        RegMap::create_packed(
            VIRTIO_BLK_CFG_SIZE,
            &layout,
            Some(BlockReg::Unused),
        )
    };
}

mod bits {
    #![allow(unused)]

    pub const VIRTIO_BLK_T_IN: u32 = 0;
    pub const VIRTIO_BLK_T_OUT: u32 = 1;
    pub const VIRTIO_BLK_T_FLUSH: u32 = 4;
    pub const VIRTIO_BLK_T_DISCARD: u32 = 11;
    pub const VIRTIO_BLK_T_WRITE_ZEROES: u32 = 13;

    pub const VIRTIO_BLK_S_OK: u8 = 0;
    pub const VIRTIO_BLK_S_IOERR: u8 = 1;
    pub const VIRTIO_BLK_S_UNSUPP: u8 = 2;

    pub const VIRTIO_BLK_CFG_SIZE: usize = 0x3c;
}

#[usdt::provider(provider = "propolis")]
mod probes {
    fn vioblk_read_enqueue(id: u16, off: u64, sz: u64) {}
    fn vioblk_read_complete(id: u16, res: u8) {}

    fn vioblk_write_enqueue(id: u16, off: u64, sz: u64) {}
    fn vioblk_write_complete(id: u16, res: u8) {}

    fn vioblk_flush_enqueue(id: u16) {}
    fn vioblk_flush_complete(id: u16, res: u8) {}
}
