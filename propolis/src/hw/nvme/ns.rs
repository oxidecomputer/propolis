use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use crate::block::*;
use crate::hw::nvme::bits::RawCompletion;
use crate::hw::nvme::cmds::{self, NvmCmd};
use crate::{common, dispatch::DispCtx};

use super::bits::{self, RawSubmission};
use super::cmds::{Completion, ReadCmd, WriteCmd};
use super::queue::{CompQueue, SubQueue};
use super::NvmeError;

/// Supported block size.
/// TODO: Support more
const BLOCK_SZ: u64 = 512;

/// NVMe Namespace with underlying block device
pub struct NvmeNs {
    /// The Identify structure returned for Identify namespace commands
    pub ident: bits::IdentifyNamespace,

    /// The underlying block device to service read/write requests
    bdev: Arc<dyn BlockDev<Request>>,

    /// Whether the underlying block device readonly
    is_ro: bool,
}

impl NvmeNs {
    /// Create a new NVMe namespace with the given block device
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

        debug_assert_eq!(
            BLOCK_SZ.count_ones(),
            1,
            "BLOCK_SZ must be a power of 2"
        );
        debug_assert!(
            BLOCK_SZ.trailing_zeros() >= 9,
            "BLOCK_SZ must be at least 512 bytes"
        );
        ident.lbaf[0].lbads = BLOCK_SZ.trailing_zeros() as u8;

        NvmeNs { ident, bdev, is_ro: !binfo.writable }
    }

    /// Convert some number of logical blocks to bytes with the currently active LBA data size
    fn nlb_to_size(&self, b: usize) -> usize {
        b << (self.ident.lbaf[(self.ident.flbas & 0xF) as usize].lbads)
    }

    /// Takes the given list of raw IO commands and queues up reads and writes to the underlying
    /// block device as appropriate.
    pub(super) fn queue_io_cmds(
        &self,
        cmds: Vec<RawSubmission>,
        cq: Arc<Mutex<CompQueue>>,
        sq: Arc<Mutex<SubQueue>>,
        ctx: &DispCtx,
    ) -> Result<(), NvmeError> {
        for sub in cmds {
            let cmd = NvmCmd::parse(sub)?;
            match cmd {
                NvmCmd::Write(_) if self.is_ro => {
                    let mut cq = cq.lock().unwrap();
                    let sq = sq.lock().unwrap();
                    let comp = Completion::specific_err(
                        bits::StatusCodeType::CmdSpecific,
                        bits::STS_WRITE_READ_ONLY_RANGE,
                    );
                    let completion = RawCompletion {
                        dw0: comp.dw0,
                        rsvd: 0,
                        sqhd: sq.head(),
                        sqid: sq.id(),
                        cid: sub.cid(),
                        status_phase: comp.status | cq.phase(),
                    };

                    cq.push(completion, ctx);
                }
                NvmCmd::Write(cmd) => {
                    self.write_cmd(sub.cid(), cmd, ctx, cq.clone(), sq.clone())
                }
                NvmCmd::Read(cmd) => {
                    self.read_cmd(sub.cid(), cmd, ctx, cq.clone(), sq.clone())
                }
                NvmCmd::Flush | NvmCmd::Unknown(_) => {
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
                        dw0: comp.dw0,
                        rsvd: 0,
                        sqhd: sq.head(),
                        sqid: sq.id(),
                        cid: sub.cid(),
                        status_phase: comp.status | cq.phase(),
                    };

                    cq.push(completion, ctx);
                }
            }
        }

        Ok(())
    }

    /// Enqueues a read to the underlying block device
    fn read_cmd(
        &self,
        cid: u16,
        cmd: ReadCmd,
        ctx: &DispCtx,
        cq: Arc<Mutex<CompQueue>>,
        sq: Arc<Mutex<SubQueue>>,
    ) {
        let off = self.nlb_to_size(cmd.slba as usize);
        let size = self.nlb_to_size(cmd.nlb as usize);
        // TODO: handles if it gets unmapped?
        let bufs = cmd.data(size as u64, ctx.mctx.memctx()).collect();
        self.bdev.enqueue(Request {
            op: BlockOp::Read,
            off,
            xfer_left: size,
            bufs,
            cid,
            cq,
            sq,
        });
    }

    /// Enqueues a write to the underlying block device
    fn write_cmd(
        &self,
        cid: u16,
        cmd: WriteCmd,
        ctx: &DispCtx,
        cq: Arc<Mutex<CompQueue>>,
        sq: Arc<Mutex<SubQueue>>,
    ) {
        let off = self.nlb_to_size(cmd.slba as usize);
        let size = self.nlb_to_size(cmd.nlb as usize);
        // TODO: handles if it gets unmapped?
        let bufs = cmd.data(size as u64, ctx.mctx.memctx()).collect();
        self.bdev.enqueue(Request {
            op: BlockOp::Write,
            off,
            xfer_left: size,
            bufs,
            cid,
            cq,
            sq,
        });
    }
}

/// I/O Request to block device
pub struct Request {
    /// The operation type
    op: BlockOp,

    /// The offset at which to begin reading/writing
    off: usize,

    /// How many bytes to read/write
    xfer_left: usize,

    /// The buffers to read/write from/to
    bufs: VecDeque<common::GuestRegion>,

    /// The associated command id
    cid: u16,

    /// The associated Completion Queue
    cq: Arc<Mutex<CompQueue>>,

    /// The associated Submission Queue
    sq: Arc<Mutex<SubQueue>>,
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
            BlockResult::Failure => {
                cmds::Completion::generic_err(bits::STS_DATA_XFER_ERR)
            }
            BlockResult::Unsupported => cmds::Completion::specific_err(
                bits::StatusCodeType::CmdSpecific,
                bits::STS_READ_CONFLICTING_ATTRS,
            ),
        };

        let sq = self.sq.lock().unwrap();
        let mut cq = self.cq.lock().unwrap();

        let completion = bits::RawCompletion {
            dw0: comp.dw0,
            rsvd: 0,
            sqhd: sq.head(),
            sqid: sq.id(),
            cid: self.cid,
            status_phase: comp.status | cq.phase(),
        };

        cq.push(completion, ctx);

        // TODO: should this be done here?
        cq.fire_interrupt(ctx);
    }
}
