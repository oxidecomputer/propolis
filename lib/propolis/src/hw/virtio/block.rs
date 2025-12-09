// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::Instant;

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

/// Arbitrary limit to sectors permitted per discard request
const MAX_DISCARD_SECTORS: u32 = ((1024 * 1024) / SECTOR_SZ) as u32;

pub struct PciVirtioBlock {
    virtio_state: PciVirtioState,
    pci_state: pci::DeviceState,
    pub block_attach: block::DeviceAttachment,
}
impl PciVirtioBlock {
    pub fn new(queue_size: u16) -> Arc<Self> {
        let queues =
            VirtQueues::new([VirtQueue::new(queue_size.try_into().unwrap())])
                .unwrap();
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

        let block_attach = block::DeviceAttachment::new(
            NonZeroUsize::new(1).unwrap(),
            pci_state.acc_mem.child(Some("block backend".to_string())),
        );
        let bvq = BlockVq::new(
            virtio_state.queues.get(0).unwrap().clone(),
            pci_state.acc_mem.child(Some("block queue".to_string())),
        );
        block_attach.queue_associate(0usize.into(), bvq);

        Arc::new(Self { pci_state, virtio_state, block_attach })
    }

    fn block_cfg_read(&self, id: &BlockReg, ro: &mut ReadOp) {
        let info = self.block_attach.info().unwrap_or_else(Default::default);

        let total_bytes = info.total_size * u64::from(info.block_size);
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
            BlockReg::MaxDiscardSectors => {
                // Arbitrarily limit to 1MiB (or the device size, if smaller)
                let sz = u32::min(
                    MAX_DISCARD_SECTORS,
                    (info.total_size / SECTOR_SZ as u64) as u32,
                );
                ro.write_u32(if info.supports_discard { sz } else { 0 });
            }
            BlockReg::MaxDiscardSeg => {
                // If the device supports discard operations, only permit one
                // segment (LBA/size) per request.
                ro.write_u32(if info.supports_discard { 1 } else { 0 });
            }
            BlockReg::DiscardSectorAlign => {
                // Expect that discard operations are block-aligned
                ro.write_u32(if info.supports_discard {
                    info.block_size / SECTOR_SZ as u32
                } else {
                    0
                });
            }
            _ => {
                // XXX: all zeroes for now
                ro.fill(0);
            }
        }
    }
}

struct CompletionToken {
    /// ID of original request.
    rid: u16,
    /// VirtIO chain in which we indicate the result.
    chain: Chain,
}

struct BlockVq(Arc<VirtQueue>, MemAccessor);
impl BlockVq {
    fn new(vq: Arc<VirtQueue>, acc_mem: MemAccessor) -> Arc<Self> {
        Arc::new(Self(vq, acc_mem))
    }
}
impl block::DeviceQueue for BlockVq {
    type Token = CompletionToken;

    fn next_req(
        &self,
    ) -> Option<(block::Request, Self::Token, Option<Instant>)> {
        let vq = &self.0;
        let mem = self.1.access()?;

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
                    Ok((
                        block::Request::new_read(off, sz, regions),
                        CompletionToken { rid, chain },
                        None,
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
                    Ok((
                        block::Request::new_write(off, sz, regions),
                        CompletionToken { rid, chain },
                        None,
                    ))
                } else {
                    Err(chain)
                }
            }
            VIRTIO_BLK_T_FLUSH => {
                probes::vioblk_flush_enqueue!(|| rid);
                Ok((
                    block::Request::new_flush(),
                    CompletionToken { rid, chain },
                    None,
                ))
            }
            VIRTIO_BLK_T_DISCARD => {
                let mut detail = DiscardWriteZeroes::default();
                if !chain.read(&mut detail, &mem) {
                    Err(chain)
                } else {
                    let off = detail.sector as usize * SECTOR_SZ;
                    let sz = detail.num_sectors as usize * SECTOR_SZ;
                    probes::vioblk_discard_enqueue!(|| (
                        rid, off as u64, sz as u64,
                    ));
                    Ok((
                        block::Request::new_discard(off, sz),
                        CompletionToken { rid, chain },
                        None,
                    ))
                }
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

    fn complete(
        &self,
        op: block::Operation,
        result: block::Result,
        mut token: Self::Token,
    ) {
        let CompletionToken { rid, ref mut chain } = token;
        if let Some(mem) = self.1.access() {
            let resnum = match result {
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
                block::Operation::Discard(..) => {
                    probes::vioblk_discard_complete!(|| (rid, resnum));
                }
            }
            chain.write(&resnum, &mem);
            self.0.push_used(chain, &mem);
        }
    }

    fn abandon(&self, _token: Self::Token) {
        // Nothing necessary to safely abandon a `CompletionToken`.
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
        if info.supports_discard {
            feat |= VIRTIO_BLK_F_DISCARD;
        }
        feat
    }
    fn set_features(&self, _feat: u32) -> Result<(), ()> {
        // XXX: real features
        Ok(())
    }

    fn queue_notify(&self, _vq: &Arc<VirtQueue>) {
        // TODO: provide proper hint
        self.block_attach.notify(0usize.into(), None);
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
    fn attachment(&self) -> &block::DeviceAttachment {
        &self.block_attach
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
        self.block_attach.pause()
    }
    fn resume(&self) {
        self.block_attach.resume();
    }
    fn paused(&self) -> BoxFuture<'static, ()> {
        Box::pin(self.block_attach.none_processing())
    }
    fn migrate(&self) -> Migrator<'_> {
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

#[derive(Copy, Clone, Debug, Default)]
#[repr(C)]
struct DiscardWriteZeroes {
    sector: u64,
    num_sectors: u32,
    flags: u32,
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

    fn vioblk_discard_enqueue(id: u16, off: u64, sz: u64) {}
    fn vioblk_discard_complete(id: u16, res: u8) {}
}
