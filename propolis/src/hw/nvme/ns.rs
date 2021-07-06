use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use crate::block::*;
use crate::hw::nvme::bits::RawCompletion;
use crate::hw::nvme::cmds::{self, NvmCmd};
use crate::{common, dispatch::DispCtx};

use super::NvmeError;
use super::bits::{self, RawSubmission};
use super::cmds::{Completion, ReadCmd, WriteCmd};
use super::queue::{CompQueue, SubQueue};

const BLOCK_SZ: u64 = 512;

pub struct NvmeNs {
    pub ident: bits::IdentifyNamespace,
    bdev: Arc<dyn BlockDev<Request>>,
}

impl NvmeNs {
    pub fn create(bdev: Arc<dyn BlockDev<Request>>) -> Self {
        let binfo = bdev.inquire();
        let total_bytes = binfo.total_size * binfo.block_size as u64;
        let nsze = total_bytes / BLOCK_SZ;

        let mut ident = bits::IdentifyNamespace {
            // No thin provisioning so nsze == ncap == nuse
            nsze,
            ncap: nsze,
            nuse: nsze,
            nlbaf: 0, // We only support a single LBA format (1 but 0-based)
            flbas: 0, // And it is at index 0 in the lbaf array
            ..Default::default()
        };

        debug_assert_eq!(BLOCK_SZ.count_ones(), 1, "BLOCK_SZ must be a power of 2");
        debug_assert!(BLOCK_SZ.trailing_zeros() >= 9, "BLOCK_SZ must be at least 512 bytes");
        ident.lbaf[0].lbads = BLOCK_SZ.trailing_zeros() as u8;

        NvmeNs { ident, bdev }
    }

    /// Convert some number of logical blocks to bytes with the currently active LBA data size
    fn nlb_to_size(&self, b: usize) -> usize {
        b << (self.ident.lbaf[(self.ident.flbas & 0xF) as usize].lbads)
    }

    /// Takes the given list of raw IO commands and queues up reads and writes to the underlying
    /// block device as appropriate.
    pub fn queue_io_cmds(
        &self,
        cmds: Vec<RawSubmission>,
        cq: Arc<Mutex<CompQueue>>,
        sq: Arc<Mutex<SubQueue>>,
        ctx: &DispCtx
    ) -> Result<(), NvmeError> {
        for cmd in cmds {
            let (cmd, sub) = NvmCmd::parse(cmd)?;
            match cmd {
                NvmCmd::Write(cmd) => self.write_cmd(sub.cid, cmd, ctx, cq.clone(), sq.clone()),
                NvmCmd::Read(cmd) => self.read_cmd(sub.cid, cmd, ctx, cq.clone(), sq.clone()),
                NvmCmd::Flush |
                NvmCmd::Unknown(_) => {
                    // For any other command, just immediately complete it
                    let mut cq = cq.lock().unwrap();
                    let sq = sq.lock().unwrap();

                    let comp = if matches!(cmd, NvmCmd::Flush) {
                        // TODO: is there anything else to do for flush?
                        Completion::success()
                    } else {
                        Completion::generic_err(bits::STS_INTERNAL_ERR)
                    };

                    let completion = RawCompletion {
                        cdw0: comp.cdw0,
                        rsvd: 0,
                        sqhd: sq.head(),
                        sqid: sq.id(),
                        cid: sub.cid,
                        status: comp.status | cq.phase()
                    };

                    cq.push(completion, ctx);
                }
            }
        }

        Ok(())
    }

    fn read_cmd(
        &self,
        cid: u16,
        mut cmd: ReadCmd,
        ctx: &DispCtx,
        cq: Arc<Mutex<CompQueue>>,
        sq: Arc<Mutex<SubQueue>>
    ) {
        // `nlb` is a 0-based value and so add 1 to get the corresponding value
        cmd.nlb += 1;
        let off = self.nlb_to_size(cmd.slba as usize);
        let size = self.nlb_to_size(cmd.nlb as usize);
        // TODO: handles if it gets unmapped?
        let bufs = cmd.data(size as u64, ctx.mctx.memctx()).collect();
        self.bdev.enqueue(Request::new_read(off, size, bufs, cid, cq, sq));
    }

    fn write_cmd(
        &self,
        cid: u16,
        mut cmd: WriteCmd,
        ctx: &DispCtx,
        cq: Arc<Mutex<CompQueue>>,
        sq: Arc<Mutex<SubQueue>>
    ) {
        // `nlb` is a 0-based value and so add 1 to get the corresponding value
        cmd.nlb += 1;
        let off = self.nlb_to_size(cmd.slba as usize);
        let size = self.nlb_to_size(cmd.nlb as usize);
        // TODO: handles if it gets unmapped?
        let bufs = cmd.data(size as u64, ctx.mctx.memctx()).collect();
        self.bdev.enqueue(Request::new_write(off, size, bufs, cid, cq, sq));
    }
}

pub struct Request {
    op: BlockOp,
    off: usize,
    xfer_left: usize,
    bufs: VecDeque<common::GuestRegion>,
    cid: u16,
    cq: Arc<Mutex<CompQueue>>,
    sq: Arc<Mutex<SubQueue>>,
}

impl Request {
    fn new_read(
        off: usize,
        size: usize,
        bufs: VecDeque<common::GuestRegion>,
        cid: u16,
        cq: Arc<Mutex<CompQueue>>,
        sq: Arc<Mutex<SubQueue>>
    ) -> Self {
        Self {
            op: BlockOp::Read,
            off,
            xfer_left: size,
            bufs,
            cid,
            cq,
            sq
        }
    }

    fn new_write(
        off: usize,
        size: usize,
        bufs: VecDeque<common::GuestRegion>,
        cid: u16,
        cq: Arc<Mutex<CompQueue>>,
        sq: Arc<Mutex<SubQueue>>
    ) -> Self {
        Self {
            op: BlockOp::Write,
            off,
            xfer_left: size,
            bufs,
            cid,
            cq,
            sq
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

    fn next_buf(&mut self) -> Option<common::GuestRegion> {
        if self.xfer_left == 0 {
            return None;
        }

        if let Some(mut region) = self.bufs.pop_front() {
            if region.1 > self.xfer_left {
                region.1 = self.xfer_left;
            }
            self.xfer_left -= region.1;
            Some(region)
        } else {
            None
        }
    }

    fn complete(self, res: BlockResult, ctx: &DispCtx) {
        let comp = match res {
            BlockResult::Success => cmds::Completion::success(),
            BlockResult::Failure => cmds::Completion::generic_err(bits::STS_DATA_XFER_ERR),
            BlockResult::Unsupported => cmds::Completion::specific_err(
                bits::StatusCodeType::CmdSpecific,
                bits::STS_READ_CONFLICTING_ATTRS
            )
        };

        let sq = self.sq.lock().unwrap();
        let mut cq = self.cq.lock().unwrap();

        let completion = bits::RawCompletion {
            cdw0: comp.cdw0,
            rsvd: 0,
            sqhd: sq.head(),
            sqid: sq.id(),
            cid: self.cid,
            status: comp.status | cq.phase(),
        };

        cq.push(completion, ctx);

        // TODO: should this be done here?
        cq.fire_interrupt(ctx);
    }
}