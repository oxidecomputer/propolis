use std::sync::Arc;

use crate::block::*;
use crate::common::*;
use crate::dispatch::DispCtx;
use crate::hw::pci;
use crate::util::regmap::RegMap;

use super::bits::*;
use super::pci::PciVirtio;
use super::queue::{Chain, VirtQueue};
use super::VirtioDevice;

use lazy_static::lazy_static;

/// Sizing for virtio-block is specified in 512B sectors
const SECTOR_SZ: usize = 512;

pub struct VirtioBlock {
    bdev: Arc<dyn BlockDev<Request>>,
}
impl VirtioBlock {
    pub fn create(
        queue_size: u16,
        bdev: Arc<dyn BlockDev<Request>>,
    ) -> Arc<pci::DeviceInst> {
        // virtio-block only needs two MSI-X entries for its interrupt needs:
        // - device config changes
        // - queue 0 notification
        let msix_count = Some(2);

        PciVirtio::create(
            queue_size,
            1,
            msix_count,
            VIRTIO_DEV_BLOCK,
            pci::bits::CLASS_STORAGE,
            VIRTIO_BLK_CFG_SIZE,
            Arc::new(Self { bdev }),
        )
    }

    fn block_cfg_read(&self, id: &BlockReg, ro: &mut ReadOp) {
        let info = self.bdev.inquire();
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

        let dev_data = self.bdev.inquire();
        if !dev_data.writable {
            feat |= VIRTIO_BLK_F_RO;
        }
        feat
    }
    fn device_set_features(&self, _feat: u32) {
        // XXX: real features
    }

    fn queue_notify(&self, vq: &Arc<VirtQueue>, ctx: &DispCtx) {
        let mem = &ctx.mctx.memctx();

        loop {
            let mut chain = Chain::with_capacity(4);
            let clen = vq.pop_avail(&mut chain, mem);
            if clen.is_none() {
                break;
            }

            let mut breq = VbReq::default();
            if !chain.read(&mut breq, mem) {
                todo!("error handling");
            }
            match breq.rtype {
                VIRTIO_BLK_T_IN => {
                    // should be (blocksize * 512) + 1 remaining write bytes
                    let remain = chain.remain_write_bytes();
                    let blocks = (remain - 1) / SECTOR_SZ;

                    self.bdev.enqueue(Request::new_read(
                        chain,
                        Arc::clone(vq),
                        breq.sector as usize * SECTOR_SZ,
                        blocks * SECTOR_SZ,
                    ));
                }
                VIRTIO_BLK_T_OUT => {
                    // should be (blocksize * 512) remaining read bytes
                    let blocks = chain.remain_read_bytes() / SECTOR_SZ;
                    self.bdev.enqueue(Request::new_write(
                        chain,
                        Arc::clone(vq),
                        breq.sector as usize * SECTOR_SZ,
                        blocks * SECTOR_SZ,
                    ));
                }
                _ => {
                    // try to set the status byte to failed
                    let remain = chain.remain_write_bytes();
                    if remain >= 1 {
                        chain.write_skip(remain - 1);
                        chain.write(&VIRTIO_BLK_S_UNSUPP, mem);
                    }
                    vq.push_used(&mut chain, mem, ctx);
                }
            }
        }
    }
}

pub struct Request {
    op: BlockOp,
    off: usize,
    xfer_left: usize,
    xfer_used: usize,
    chain: Chain,
    vq: Arc<VirtQueue>,
}

impl Request {
    fn new_read(
        chain: Chain,
        vq: Arc<VirtQueue>,
        off: usize,
        size: usize,
    ) -> Self {
        assert_eq!(chain.remain_write_bytes(), size + 1);
        Self {
            op: BlockOp::Read,
            off,
            xfer_left: size,
            xfer_used: 0,
            chain,
            vq,
        }
    }
    fn new_write(
        chain: Chain,
        vq: Arc<VirtQueue>,
        off: usize,
        size: usize,
    ) -> Self {
        assert_eq!(chain.remain_read_bytes(), size);
        assert_eq!(chain.remain_write_bytes(), 1);
        Self {
            op: BlockOp::Write,
            off,
            xfer_left: size,
            xfer_used: 0,
            chain,
            vq,
        }
    }
}

impl BlockReq for Request {
    fn oper(&self) -> BlockOp {
        self.op
    }

    fn offset(&self) -> usize {
        self.off
    }

    fn next_buf(&mut self) -> Option<GuestRegion> {
        if self.xfer_left == 0 {
            return None;
        }
        let res = match self.op {
            BlockOp::Flush => return None,
            BlockOp::Read => self.chain.writable_buf(self.xfer_left),
            BlockOp::Write => self.chain.readable_buf(self.xfer_left),
        };
        if let Some(region) = res.as_ref() {
            assert!(self.xfer_left >= region.1);
            self.xfer_left -= region.1;
            self.xfer_used += region.1;
        }
        res
    }

    fn complete(mut self, res: BlockResult, ctx: &DispCtx) {
        assert_eq!(self.chain.remain_write_bytes(), 1);
        let mem = &ctx.mctx.memctx();
        match res {
            BlockResult::Success => self.chain.write(&VIRTIO_BLK_S_OK, mem),
            BlockResult::Failure => self.chain.write(&VIRTIO_BLK_S_IOERR, mem),
            BlockResult::Unsupported => {
                self.chain.write(&VIRTIO_BLK_S_UNSUPP, mem)
            }
        };
        self.vq.push_used(&mut self.chain, mem, ctx);
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
use bits::*;
