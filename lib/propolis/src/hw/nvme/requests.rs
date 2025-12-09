// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::sync::Arc;
use std::time::Instant;

use super::{cmds::NvmCmd, queue::Permit, PciNvme};
use crate::accessors::MemAccessor;
use crate::block::{self, Operation, Request};
use crate::hw::nvme::{bits, cmds::Completion, queue::SubQueue};

#[usdt::provider(provider = "propolis")]
mod probes {
    fn nvme_read_enqueue(qid: u16, idx: u16, cid: u16, off: u64, sz: u64) {}
    fn nvme_read_complete(qid: u16, cid: u16, res: u8) {}

    fn nvme_write_enqueue(qid: u16, idx: u16, cid: u16, off: u64, sz: u64) {}
    fn nvme_write_complete(qid: u16, cid: u16, res: u8) {}

    fn nvme_flush_enqueue(qid: u16, idx: u16, cid: u16) {}
    fn nvme_flush_complete(qid: u16, cid: u16, res: u8) {}

    fn nvme_raw_cmd(
        qid: u16,
        cdw0nsid: u64,
        prp1: u64,
        prp2: u64,
        cdw10cdw11: u64,
    ) {
    }
}

impl block::Device for PciNvme {
    fn attachment(&self) -> &block::DeviceAttachment {
        &self.block_attach
    }
}

pub(super) struct NvmeBlockQueue {
    sq: Arc<SubQueue>,
    acc_mem: MemAccessor,
}
impl NvmeBlockQueue {
    pub(super) fn new(sq: Arc<SubQueue>, acc_mem: MemAccessor) -> Arc<Self> {
        Arc::new(Self { sq, acc_mem })
    }
}
impl block::DeviceQueue for NvmeBlockQueue {
    type Token = Permit;

    /// Pop an available I/O request off of the Submission Queue for hand-off to
    /// the underlying block backend
    fn next_req(&self) -> Option<(Request, Self::Token, Option<Instant>)> {
        let sq = &self.sq;
        let mem = self.acc_mem.access()?;
        let params = self.sq.params();

        while let Some((sub, permit, idx)) = sq.pop() {
            let qid = sq.id();
            probes::nvme_raw_cmd!(|| {
                (
                    qid,
                    u64::from(sub.cdw0) | (u64::from(sub.nsid) << 32),
                    sub.prp1,
                    sub.prp2,
                    (u64::from(sub.cdw10) | (u64::from(sub.cdw11) << 32)),
                )
            });
            let cid = sub.cid();
            let cmd = NvmCmd::parse(sub);

            match cmd {
                Ok(NvmCmd::Write(cmd)) => {
                    let off = params.lba_data_size * cmd.slba;
                    let size = params.lba_data_size * (cmd.nlb as u64);

                    if size > params.max_data_tranfser_size {
                        permit.complete(
                            Completion::generic_err(bits::STS_INVAL_FIELD)
                                .dnr(),
                        );
                        continue;
                    }

                    probes::nvme_write_enqueue!(|| (qid, idx, cid, off, size));

                    let bufs = cmd.data(size, &mem).collect();
                    let req =
                        Request::new_write(off as usize, size as usize, bufs);
                    return Some((req, permit, None));
                }
                Ok(NvmCmd::Read(cmd)) => {
                    let off = params.lba_data_size * cmd.slba;
                    let size = params.lba_data_size * (cmd.nlb as u64);

                    if size > params.max_data_tranfser_size {
                        permit.complete(
                            Completion::generic_err(bits::STS_INVAL_FIELD)
                                .dnr(),
                        );
                        continue;
                    }

                    probes::nvme_read_enqueue!(|| (qid, idx, cid, off, size));

                    let bufs = cmd.data(size, &mem).collect();
                    let req =
                        Request::new_read(off as usize, size as usize, bufs);
                    return Some((req, permit, None));
                }
                Ok(NvmCmd::Flush) => {
                    probes::nvme_flush_enqueue!(|| (qid, idx, cid));
                    let req = Request::new_flush();
                    return Some((req, permit, None));
                }
                Ok(NvmCmd::Unknown(_)) | Err(_) => {
                    // For any other unrecognized or malformed command,
                    // just immediately complete it with an error
                    let comp = Completion::generic_err(bits::STS_INTERNAL_ERR);
                    permit.complete(comp);
                }
            }
        }
        None
    }

    /// Place the operation result (success or failure) onto the corresponding
    /// Completion Queue.
    fn complete(
        &self,
        op: block::Operation,
        result: block::Result,
        permit: Self::Token,
    ) {
        let qid = permit.sqid();
        let cid = permit.cid();
        let resnum = result as u8;
        match op {
            Operation::Read(..) => {
                probes::nvme_read_complete!(|| (qid, cid, resnum));
            }
            Operation::Write(..) => {
                probes::nvme_write_complete!(|| (qid, cid, resnum));
            }
            Operation::Flush => {
                probes::nvme_flush_complete!(|| (qid, cid, resnum));
            }
            Operation::Discard(..) => {
                unreachable!("discard not supported in NVMe for now");
            }
        }

        permit.complete(Completion::from(result));
    }

    /// In the unlikely case we must give up on an in-flight I/O, tear it down
    /// without triggering the no-drop check on NVMe request permits.
    fn abandon(&self, token: Self::Token) {
        token.abandon();
    }
}
