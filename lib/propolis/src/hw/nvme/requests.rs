// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use crate::{
    accessors::{opt_guard, MemAccessor},
    block::{self, BlockPayload, Operation, Request, Result as BlockResult},
    hw::nvme::{bits, cmds::Completion},
};

use super::{cmds::NvmCmd, queue::CompQueueEntryPermit, PciNvme};

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
    fn next(&self) -> Option<Request> {
        self.notifier.next_arming(|| self.next_req())
    }

    fn complete(
        &self,
        op: Operation,
        res: BlockResult,
        payload: Box<BlockPayload>,
    ) {
        let payload: Box<CompletionPayload> =
            payload.downcast().expect("payload must be correct type");
        let CompletionPayload { qid, cid, cqe_permit } = *payload;
        self.complete_req(qid, cid, op, res, cqe_permit);
    }

    fn accessor_mem(&self) -> MemAccessor {
        self.pci_state.acc_mem.child()
    }

    fn set_notifier(&self, f: Option<Box<block::NotifierFn>>) {
        self.notifier.set(f)
    }
}

impl PciNvme {
    /// Pop an available I/O request off of a Submission Queue to begin
    /// processing by the underlying Block Device.
    fn next_req(&self) -> Option<Request> {
        let state = self.state.lock().unwrap();

        // We shouldn't be called while paused
        assert!(!state.paused, "I/O requested while device paused");

        let mem = self.mem_access()?;

        // Go through all the queues (skip admin as we just want I/O queues)
        // looking for a request to service
        for sq in state.sqs.iter().skip(1).flatten() {
            while let Some((sub, cqe_permit, idx)) = sq.pop(&mem) {
                let qid = sq.id();
                probes::nvme_raw_cmd!(|| {
                    (
                        qid,
                        sub.cdw0 as u64 | ((sub.nsid as u64) << 32),
                        sub.prp1,
                        sub.prp2,
                        (sub.cdw10 as u64 | ((sub.cdw11 as u64) << 32)),
                    )
                });
                let cmd = NvmCmd::parse(sub);
                let cid = sub.cid();
                match cmd {
                    Ok(NvmCmd::Write(_)) if !state.binfo.writable => {
                        let comp = Completion::specific_err(
                            bits::StatusCodeType::CmdSpecific,
                            bits::STS_WRITE_READ_ONLY_RANGE,
                        );
                        cqe_permit.push_completion(cid, comp, Some(&mem));
                    }
                    Ok(NvmCmd::Write(cmd)) => {
                        let off = state.nlb_to_size(cmd.slba as usize) as u64;
                        let size = state.nlb_to_size(cmd.nlb as usize) as u64;
                        probes::nvme_write_enqueue!(|| (
                            qid, idx, cid, off, size
                        ));

                        let bufs = cmd.data(size, &mem).collect();
                        let req = Request::new_write(
                            off as usize,
                            bufs,
                            CompletionPayload::new(qid, cid, cqe_permit),
                        );
                        return Some(req);
                    }
                    Ok(NvmCmd::Read(cmd)) => {
                        let off = state.nlb_to_size(cmd.slba as usize) as u64;
                        let size = state.nlb_to_size(cmd.nlb as usize) as u64;

                        probes::nvme_read_enqueue!(|| (
                            qid, idx, cid, off, size
                        ));

                        let bufs = cmd.data(size, &mem).collect();
                        let req = Request::new_read(
                            off as usize,
                            bufs,
                            CompletionPayload::new(qid, cid, cqe_permit),
                        );
                        return Some(req);
                    }
                    Ok(NvmCmd::Flush) => {
                        probes::nvme_flush_enqueue!(|| (qid, idx, cid));
                        let req = Request::new_flush(
                            0,
                            0, // TODO: is 0 enough or do we pass total size?
                            CompletionPayload::new(qid, cid, cqe_permit),
                        );
                        return Some(req);
                    }
                    Ok(NvmCmd::Unknown(_)) | Err(_) => {
                        // For any other unrecognized or malformed command,
                        // just immediately complete it with an error
                        let comp =
                            Completion::generic_err(bits::STS_INTERNAL_ERR);
                        cqe_permit.push_completion(cid, comp, Some(&mem));
                    }
                }
            }
        }

        None
    }

    /// Place the operation result (success or failure) onto the corresponding
    /// Completion Queue.
    fn complete_req(
        &self,
        qid: u16,
        cid: u16,
        op: Operation,
        res: BlockResult,
        cqe_permit: CompQueueEntryPermit,
    ) {
        let resnum: u8 = match &res {
            BlockResult::Success => 0,
            BlockResult::Failure => 1,
            BlockResult::Unsupported => 2,
        };
        match op {
            Operation::Read(..) => {
                probes::nvme_read_complete!(|| (qid, cid, resnum));
            }
            Operation::Write(..) => {
                probes::nvme_write_complete!(|| (qid, cid, resnum));
            }
            Operation::Flush(..) => {
                probes::nvme_flush_complete!(|| (qid, cid, resnum));
            }
        }

        let mem = self.mem_access();
        cqe_permit.push_completion(cid, Completion::from(res), opt_guard(&mem));
    }
}

struct CompletionPayload {
    /// The Submission Queue ID the request was taken from.
    qid: u16,
    /// The Command ID of the original request.
    cid: u16,
    /// Entry permit for the CQ.
    cqe_permit: CompQueueEntryPermit,
}
impl CompletionPayload {
    pub(super) fn new(
        qid: u16,
        cid: u16,
        cqe_permit: CompQueueEntryPermit,
    ) -> Box<Self> {
        Box::new(Self { qid, cid, cqe_permit })
    }
}
