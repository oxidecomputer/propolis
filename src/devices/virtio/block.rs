use std::sync::Arc;

use crate::common::*;
use crate::dispatch::DispCtx;
use crate::pci;
use crate::util::regmap::RegMap;

use super::pci::PciVirtio;
use super::queue::{Chain, VirtQueue};
use super::VirtioDevice;

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

pub struct VirtioBlock {}
impl VirtioBlock {
    pub fn new(queue_size: u16) -> Arc<pci::DeviceInst<PciVirtio<Self>>> {
        PciVirtio::new(queue_size, 1, Self {})
    }

    fn block_cfg_read(&self, id: &BlockReg, ro: &mut ReadOp) {
        match id {
            BlockReg::Capacity => {}
            BlockReg::SizeMax => {}
            BlockReg::SegMax => {}
            BlockReg::GeoCyl => {}
            BlockReg::GeoHeads => {}
            BlockReg::GeoSectors => {}
            BlockReg::BlockSize => {}
            BlockReg::TopoPhysExp => {}
            BlockReg::TopoAlignOff => {}
            BlockReg::TopoMinIoSz => {}
            BlockReg::TopoOptIoSz => {}
            BlockReg::Writeback => {}
            BlockReg::MaxDiscardSectors => {}
            BlockReg::MaxDiscardSeg => {}
            BlockReg::DiscardSectorAlign => {}
            BlockReg::MaxZeroSectors => {}
            BlockReg::MaxZeroSeg => {}
            BlockReg::ZeroMayUnmap => {}
            BlockReg::Unused => {
                for b in ro.buf.iter_mut() {
                    *b = 0;
                }
            }
        }
        // XXX: all zeroes for now
        for b in ro.buf.iter_mut() {
            *b = 0;
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
        // XXX: real features
        0
    }
    fn device_set_features(&self, feat: u32) {
        // XXX: real features
    }

    fn device_id_and_class() -> (u16, u8) {
        // block device, storage class
        (VIRTIO_DEV_BLOCK, 0x01)
    }

    fn queue_notify(&self, qid: u16, vq: &VirtQueue, ctx: &DispCtx) {
        let mem = &ctx.mctx.memctx();
        let mut chain = Chain::with_capacity(4);

        while let Some(len) = vq.pop_avail(&mut chain, mem) {
            println!("chain len {}: {:?}", len, &chain);
            let mut breq = VbReq::default();
            if !chain.read(&mut breq, mem) {
                todo!("error handling");
            }
            println!("breq {:?}", &breq);
            match breq.rtype {
                // VIRTIO_BLK_T_IN => { }
                // VIRTIO_BLK_T_OUT => { }
                _ => {
                    // try to set the status byte to failed
                    let remain = chain.remain_write_bytes();
                    if remain >= 1 {
                        chain.write_skip(remain - 1);
                        chain.write(&VIRTIO_BLK_S_UNSUPP, mem);
                    }
                }
            }
            vq.push_used(&mut chain, mem);
        }
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
