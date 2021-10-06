use std::num::NonZeroU16;
use std::sync::Arc;

use crate::block;
use crate::common::*;
use crate::dispatch::DispCtx;
use crate::hw::pci;
use crate::util::regmap::RegMap;

use super::bits::*;
use super::pci::PciVirtio;
use super::queue::{Chain, VirtQueue, VirtQueues};
use super::VirtioDevice;

use lazy_static::lazy_static;

/// Sizing for virtio-block is specified in 512B sectors
const SECTOR_SZ: usize = 512;

pub struct VirtioBlock {
    info: block::DeviceInfo,
    queues: VirtQueues,
    notifier: block::Notifier,
}
impl VirtioBlock {
    pub fn create(queue_size: u16, info: block::DeviceInfo) -> Arc<PciVirtio> {
        // virtio-block only needs two MSI-X entries for its interrupt needs:
        // - device config changes
        // - queue 0 notification
        let msix_count = Some(2);

        let queues = VirtQueues::new(
            NonZeroU16::new(queue_size).unwrap(),
            NonZeroU16::new(1).unwrap(),
        );

        let notifier = block::Notifier::new();
        PciVirtio::create(
            msix_count,
            VIRTIO_DEV_BLOCK,
            pci::bits::CLASS_STORAGE,
            VIRTIO_BLK_CFG_SIZE,
            Arc::new(Self { info, queues, notifier }),
        )
    }

    fn block_cfg_read(&self, id: &BlockReg, ro: &mut ReadOp) {
        let info = self.info;
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

    fn next_req(&self, ctx: &DispCtx) -> Option<block::Request> {
        let vq = &self.queues[0];
        let mem = &ctx.mctx.memctx();

        let mut chain = Chain::with_capacity(4);
        let _clen = vq.pop_avail(&mut chain, mem)?;

        let mut breq = VbReq::default();
        if !chain.read(&mut breq, mem) {
            todo!("error handling");
        }
        let req = match breq.rtype {
            VIRTIO_BLK_T_IN => {
                // should be (blocksize * 512) + 1 remaining writable byte for status
                // TODO: actually enforce block size
                let blocks = (chain.remain_write_bytes() - 1) / SECTOR_SZ;

                if let Some(regions) = chain.writable_bufs(blocks * SECTOR_SZ) {
                    let mvq = Arc::clone(vq);
                    Ok(block::Request::new_read(
                        breq.sector as usize * SECTOR_SZ,
                        regions,
                        Box::new(move |_op, res, ctx| {
                            complete_blockreq(res, chain, mvq, ctx);
                        }),
                    ))
                } else {
                    Err(chain)
                }
            }
            VIRTIO_BLK_T_OUT => {
                // should be (blocksize * 512) remaining read bytes
                let blocks = chain.remain_read_bytes() / SECTOR_SZ;

                if let Some(regions) = chain.readable_bufs(blocks * SECTOR_SZ) {
                    let mvq = Arc::clone(vq);
                    Ok(block::Request::new_write(
                        breq.sector as usize * SECTOR_SZ,
                        regions,
                        Box::new(move |_op, res, ctx| {
                            complete_blockreq(res, chain, mvq, ctx);
                        }),
                    ))
                } else {
                    Err(chain)
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
                    chain.write(&VIRTIO_BLK_S_UNSUPP, mem);
                }
                vq.push_used(&mut chain, mem, ctx);
                None
            }
            Ok(r) => Some(r),
        }
    }
}
impl VirtioDevice for VirtioBlock {
    fn device_cfg_rw(&self, mut rwo: RWOp) {
        BLOCK_DEV_REGS.process(&mut rwo, |id, rwo| match rwo {
            RWOp::Read(ro) => self.block_cfg_read(id, ro),
            RWOp::Write(_) => {
                //ignore writes
            }
        });
    }
    fn device_get_features(&self) -> u32 {
        let mut feat = VIRTIO_BLK_F_BLK_SIZE;
        feat |= VIRTIO_BLK_F_SEG_MAX;

        if !self.info.writable {
            feat |= VIRTIO_BLK_F_RO;
        }
        feat
    }
    fn device_set_features(&self, _feat: u32) {
        // XXX: real features
    }

    fn queue_notify(&self, _vq: &Arc<VirtQueue>, ctx: &DispCtx) {
        self.notifier.notify(self, ctx);
    }
    fn queues(&self) -> &VirtQueues {
        &self.queues
    }
}

fn complete_blockreq(
    res: block::Result,
    mut chain: Chain,
    vq: Arc<VirtQueue>,
    ctx: &DispCtx,
) {
    let mem = &ctx.mctx.memctx();
    let _ = match res {
        block::Result::Success => chain.write(&VIRTIO_BLK_S_OK, mem),
        block::Result::Failure => chain.write(&VIRTIO_BLK_S_IOERR, mem),
        block::Result::Unsupported => chain.write(&VIRTIO_BLK_S_UNSUPP, mem),
    };
    vq.push_used(&mut chain, mem, ctx);
}

impl block::Device for VirtioBlock {
    fn next(&self, ctx: &DispCtx) -> Option<block::Request> {
        self.notifier.next_arming(|| self.next_req(ctx))
    }

    fn set_notifier(&self, val: Option<Box<block::NotifierFn>>) {
        self.notifier.set(val);
    }
}
impl Entity for VirtioBlock {}

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
use bits::*;
