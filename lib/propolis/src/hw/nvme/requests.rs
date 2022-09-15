use crate::{
    accessors::{opt_guard, MemAccessor},
    block::{self, BlockPayload, Operation, Request, Result as BlockResult},
    hw::nvme::{bits, cmds::Completion},
};

use super::{cmds::NvmCmd, queue::CompQueueEntryPermit, PciNvme};

#[usdt::provider(provider = "propolis")]
mod probes {
    fn nvme_read_enqueue(cid: u16, off: u64, sz: u64) {}
    fn nvme_read_complete(cid: u16) {}

    fn nvme_write_enqueue(cid: u16, off: u64, sz: u64) {}
    fn nvme_write_complete(cid: u16) {}
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
        let mut payload: Box<CompletionPayload> =
            payload.downcast().expect("payload must be correct type");
        self.complete_req(op, res, &mut payload);
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
            while let Some((sub, cqe_permit)) = sq.pop(&mem) {
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
                        probes::nvme_write_enqueue!(|| (cid, off, size));

                        let bufs = cmd.data(size, &mem).collect();
                        let req = Request::new_write(
                            off as usize,
                            bufs,
                            CompletionPayload::new(cid, cqe_permit),
                        );
                        return Some(req);
                    }
                    Ok(NvmCmd::Read(cmd)) => {
                        let off = state.nlb_to_size(cmd.slba as usize) as u64;
                        let size = state.nlb_to_size(cmd.nlb as usize) as u64;

                        probes::nvme_read_enqueue!(|| (cid, off, size));

                        let bufs = cmd.data(size, &mem).collect();
                        let req = Request::new_read(
                            off as usize,
                            bufs,
                            CompletionPayload::new(cid, cqe_permit),
                        );
                        return Some(req);
                    }
                    Ok(NvmCmd::Flush) => {
                        let req = Request::new_flush(
                            0,
                            0, // TODO: is 0 enough or do we pass total size?
                            CompletionPayload::new(cid, cqe_permit),
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
        op: Operation,
        res: BlockResult,
        payload: &mut CompletionPayload,
    ) {
        let cqe_permit =
            payload.cqe_permit.take().expect("permit must be present");
        let cid = payload.cid;

        match op {
            Operation::Read(..) => {
                probes::nvme_read_complete!(|| (cid));
            }
            Operation::Write(..) => {
                probes::nvme_write_complete!(|| (cid));
            }
            _ => {}
        }

        let mem = self.mem_access();
        cqe_permit.push_completion(cid, Completion::from(res), opt_guard(&mem));
    }
}

struct CompletionPayload {
    cid: u16,
    /// Entry permit for the CQ. An option so we can `take()` it out of the `Box`
    cqe_permit: Option<CompQueueEntryPermit>,
}
impl CompletionPayload {
    pub(super) fn new(cid: u16, cqe_permit: CompQueueEntryPermit) -> Box<Self> {
        Box::new(Self { cid, cqe_permit: Some(cqe_permit) })
    }
}
