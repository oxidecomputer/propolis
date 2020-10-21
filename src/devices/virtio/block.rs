use std::sync::Arc;

use crate::block::*;
use crate::common::*;
use crate::dispatch::DispCtx;
use crate::pci;
use crate::util::regmap::RegMap;

use super::pci::PciVirtio;
use super::queue::{Chain, VirtQueue};
use super::VirtioDevice;

use byteorder::{ByteOrder, LE};
use lazy_static::lazy_static;

const VIRTIO_DEV_BLOCK: u16 = 0x1001;

const VIRTIO_BLK_T_IN: u32 = 0;
const VIRTIO_BLK_T_OUT: u32 = 1;
const VIRTIO_BLK_T_FLUSH: u32 = 4;
const VIRTIO_BLK_T_DISCARD: u32 = 11;
const VIRTIO_BLK_T_WRITE_ZEROES: u32 = 13;

const VIRTIO_BLK_S_OK: u8 = 0;
const VIRTIO_BLK_S_IOERR: u8 = 1;
const VIRTIO_BLK_S_UNSUPP: u8 = 2;

const VIRTIO_BLK_F_SIZE_MAX: u32 = 1 << 1;
const VIRTIO_BLK_F_SEG_MAX: u32 = 1 << 2;
const VIRTIO_BLK_F_GEOMETRY: u32 = 1 << 4;
const VIRTIO_BLK_F_RO: u32 = 1 << 5;
const VIRTIO_BLK_F_BLK_SIZE: u32 = 1 << 6;
const VIRTIO_BLK_F_FLUSH: u32 = 1 << 9;
const VIRTIO_BLK_F_TOPOLOGY: u32 = 1 << 10;
const VIRTIO_BLK_F_CONFIG_WCE: u32 = 1 << 11;
const VIRTIO_BLK_F_DISCARD: u32 = 1 << 13;
const VIRTIO_BLK_F_WRITE_ZEROES: u32 = 1 << 14;

/// Sizing for virtio-block is specified in 512B sectors
const SECTOR_SZ: usize = 512;

pub struct VirtioBlock {
    bdev: Arc<dyn BlockDev<Request>>,
}
impl VirtioBlock {
    pub fn new(
        queue_size: u16,
        bdev: Arc<dyn BlockDev<Request>>,
    ) -> Arc<pci::DeviceInst> {
        PciVirtio::new(queue_size, 1, Self { bdev })
    }

    fn block_cfg_read(&self, id: &BlockReg, ro: &mut ReadOp) {
        let info = self.bdev.inquire();
        let total_bytes = info.total_size * info.block_size as u64;
        match id {
            BlockReg::Capacity => {
                LE::write_u64(ro.buf, total_bytes / SECTOR_SZ as u64);
            }
            BlockReg::SegMax => {
                // XXX: only one seg per transfer allowed for now
                LE::write_u32(ro.buf, 1);
            }
            BlockReg::BlockSize => LE::write_u32(ro.buf, info.block_size),
            BlockReg::Unused => {
                for b in ro.buf.iter_mut() {
                    *b = 0;
                }
            }
            _ => {
                // XXX: all zeroes for now
                for b in ro.buf.iter_mut() {
                    *b = 0;
                }
            }
        }
    }
}
impl VirtioDevice for VirtioBlock {
    fn device_cfg_size() -> usize {
        0x3c
    }
    fn device_cfg_rw(&self, rwo: &mut RWOp) {
        BLOCK_DEV_REGS.process(rwo, |id, rwo| match rwo {
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
    fn device_set_features(&self, feat: u32) {
        // XXX: real features
    }

    fn device_id_and_class() -> (u16, u8) {
        // block device, storage class
        (VIRTIO_DEV_BLOCK, 0x01)
    }

    fn queue_notify(&self, qid: u16, vq: &Arc<VirtQueue>, ctx: &DispCtx) {
        let mem = &ctx.mctx.memctx();

        loop {
            let mut chain = Chain::with_capacity(4);
            let clen = vq.pop_avail(&mut chain, mem);
            if clen.is_none() {
                break;
            }

            let len = clen.unwrap();
            println!("chain len {}: {:?}", len, &chain);
            let mut breq = VbReq::default();
            if !chain.read(&mut breq, mem) {
                todo!("error handling");
            }
            println!("breq {:?}", &breq);
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
                    vq.push_used(&mut chain, mem);
                }
            }
        }
    }
}

pub struct Request {
    op: BlockOp,
    off: usize,
    xfer_size: usize,
    xfer_left: usize,
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
            xfer_size: size,
            xfer_left: size,
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
            xfer_size: size,
            xfer_left: size,
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
        self.vq.push_used(&mut self.chain, mem);
    }

    fn next_buf(&mut self) -> Option<GuestRegion> {
        if self.xfer_left == 0 {
            return None;
        }
        let res = match self.op {
            BlockOp::Read => self.chain.writable_buf(self.xfer_left),
            BlockOp::Write => self.chain.readable_buf(self.xfer_left),
        };
        if let Some(region) = res.as_ref() {
            assert!(self.xfer_left >= region.1);
            self.xfer_left -= region.1;
        }
        res
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
        let size = 0x3c;
        RegMap::create_packed(size, &layout, Some(BlockReg::Unused))
    };
}
