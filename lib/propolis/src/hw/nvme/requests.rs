// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use crate::{
    accessors::MemAccessor,
    block::{self, Operation, Request, Result as BlockResult},
    hw::nvme::{bits, cmds::Completion},
    vmm::mem::MemCtx,
};

use super::{cmds::NvmCmd, queue::Permit, PciNvme};

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
    fn attachment(&self) -> &block::device::Attachment {
        &self.block_attach
    }

    fn attach(&self, info: block::DeviceInfo) {
        self.state.lock().unwrap().update_block_info(info);
    }

    fn next(&self) -> Option<Request> {
        let (req, permit) = self.next_req()?;
        Some(self.block_tracking.track(req, permit))
    }

    fn complete(&self, res: BlockResult, id: block::ReqId) {
        let (op, permit) = self.block_tracking.complete(id, res);
        self.complete_req(op, res, permit);
    }

    fn accessor_mem(&self) -> MemAccessor {
        self.pci_state.acc_mem.child(Some("block backend".to_string()))
    }
}

impl PciNvme {
    /// Pop an available I/O request off of a Submission Queue to begin
    /// processing by the underlying Block Device.
    fn next_req(&self) -> Option<(Request, Permit)> {
        let state = self.state.lock().unwrap();

        let mem = self.mem_access()?;

        // Go through all the queues (skip admin as we just want I/O queues)
        // looking for a request to service
        for sq in state.sqs.iter().skip(1).flatten() {
            while let Some((sub, permit, idx)) = sq.pop(&mem) {
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
                let cid = sub.cid();
                let cmd = NvmCmd::parse(sub);

                fn fail_mdts(permit: Permit, mem: &MemCtx) {
                    permit.complete(
                        Completion::generic_err(bits::STS_INVAL_FIELD).dnr(),
                        Some(&mem),
                    );
                }

                match cmd {
                    Ok(NvmCmd::Write(cmd)) => {
                        let off = state.nlb_to_size(cmd.slba as usize) as u64;
                        let size = state.nlb_to_size(cmd.nlb as usize) as u64;

                        if !state.valid_for_mdts(size) {
                            fail_mdts(permit, &mem);
                            continue;
                        }

                        probes::nvme_write_enqueue!(|| (
                            qid, idx, cid, off, size
                        ));

                        let bufs = cmd.data(size, &mem).collect();
                        let req = Request::new_write(
                            off as usize,
                            size as usize,
                            bufs,
                        );
                        return Some((req, permit));
                    }
                    Ok(NvmCmd::Read(cmd)) => {
                        let off = state.nlb_to_size(cmd.slba as usize) as u64;
                        let size = state.nlb_to_size(cmd.nlb as usize) as u64;

                        if !state.valid_for_mdts(size) {
                            fail_mdts(permit, &mem);
                            continue;
                        }

                        probes::nvme_read_enqueue!(|| (
                            qid, idx, cid, off, size
                        ));

                        let bufs = cmd.data(size, &mem).collect();
                        let req = Request::new_read(
                            off as usize,
                            size as usize,
                            bufs,
                        );
                        return Some((req, permit));
                    }
                    Ok(NvmCmd::Flush) => {
                        probes::nvme_flush_enqueue!(|| (qid, idx, cid));
                        let req = Request::new_flush();
                        return Some((req, permit));
                    }
                    Ok(NvmCmd::Unknown(_)) | Err(_) => {
                        // For any other unrecognized or malformed command,
                        // just immediately complete it with an error
                        let comp =
                            Completion::generic_err(bits::STS_INTERNAL_ERR);
                        permit.complete(comp, Some(&mem));
                    }
                }
            }
        }

        None
    }

    /// Place the operation result (success or failure) onto the corresponding
    /// Completion Queue.
    fn complete_req(&self, op: Operation, res: BlockResult, permit: Permit) {
        let qid = permit.sqid();
        let cid = permit.cid();
        let resnum = res as u8;
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
        }

        let guard = self.mem_access();
        permit.complete(Completion::from(res), guard.as_deref());
    }
}
