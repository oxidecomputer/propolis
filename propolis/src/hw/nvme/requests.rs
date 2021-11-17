use crate::{
    block::{self, Operation, Request},
    dispatch::DispCtx,
    hw::nvme::{bits, cmds::Completion},
};

use super::{
    cmds::{self, NvmCmd},
    queue::CompQueueEntryPermit,
    NvmeCtrl, PciNvme,
};

#[usdt::provider(provider = "propolis")]
mod probes {
    fn nvme_read_enqueue(cid: u16, slba: u64, nlb: u16) {}
    fn nvme_read_complete(cid: u16) {}

    fn nvme_write_enqueue(cid: u16, slba: u64, nlb: u16) {}
    fn nvme_write_complete(cid: u16) {}
}

impl block::Device for PciNvme {
    fn next(&self, ctx: &DispCtx) -> Option<Request> {
        self.notifier.next_arming(|| self.next_req(ctx))
    }

    fn set_notifier(&self, f: Option<Box<block::NotifierFn>>) {
        self.notifier.set(f)
    }
}

impl PciNvme {
    /// Pop an available I/O request off of a Submission Queue to begin
    /// processing by the underlying Block Device.
    fn next_req(&self, ctx: &DispCtx) -> Option<Request> {
        let state = self.state.lock().unwrap();

        // Go through all the queues (skip admin as we just want I/O queues)
        // looking for a request to service
        for sq in state.sqs.iter().skip(1).flatten() {
            while let Some((sub, cqe_permit)) = sq.pop(ctx) {
                let cmd = NvmCmd::parse(sub);
                match cmd {
                    Ok(NvmCmd::Write(_)) if !state.binfo.writable => {
                        let comp = Completion::specific_err(
                            bits::StatusCodeType::CmdSpecific,
                            bits::STS_WRITE_READ_ONLY_RANGE,
                        );
                        cqe_permit.push_completion(sub.cid(), comp, ctx);
                    }
                    Ok(NvmCmd::Write(cmd)) => {
                        return Some(write_op(
                            &state,
                            sub.cid(),
                            cmd,
                            cqe_permit,
                            ctx,
                        ));
                    }
                    Ok(NvmCmd::Read(cmd)) => {
                        return Some(read_op(
                            &state,
                            sub.cid(),
                            cmd,
                            cqe_permit,
                            ctx,
                        ));
                    }
                    Ok(NvmCmd::Flush) => {
                        return Some(flush_op(sub.cid(), cqe_permit));
                    }
                    Ok(NvmCmd::Unknown(_)) | Err(_) => {
                        // For any other unrecognized or malformed command,
                        // just immediately complete it with an error
                        let comp =
                            Completion::generic_err(bits::STS_INTERNAL_ERR);
                        cqe_permit.push_completion(sub.cid(), comp, ctx);
                    }
                }
            }
        }

        None
    }
}

fn read_op(
    state: &NvmeCtrl,
    cid: u16,
    cmd: cmds::ReadCmd,
    cqe_permit: CompQueueEntryPermit,
    ctx: &DispCtx,
) -> Request {
    probes::nvme_read_enqueue!(|| (cid, cmd.slba, cmd.nlb));
    let off = state.nlb_to_size(cmd.slba as usize);
    let size = state.nlb_to_size(cmd.nlb as usize);
    let bufs = cmd.data(size as u64, ctx.mctx.memctx()).collect();
    Request::new_read(
        off,
        bufs,
        Box::new(move |op, res, ctx| {
            complete_block_req(cid, op, res, cqe_permit, ctx)
        }),
    )
}

fn write_op(
    state: &NvmeCtrl,
    cid: u16,
    cmd: cmds::WriteCmd,
    cqe_permit: CompQueueEntryPermit,
    ctx: &DispCtx,
) -> Request {
    probes::nvme_write_enqueue!(|| (cid, cmd.slba, cmd.nlb));
    let off = state.nlb_to_size(cmd.slba as usize);
    let size = state.nlb_to_size(cmd.nlb as usize);
    let bufs = cmd.data(size as u64, ctx.mctx.memctx()).collect();
    Request::new_write(
        off,
        bufs,
        Box::new(move |op, res, ctx| {
            complete_block_req(cid, op, res, cqe_permit, ctx)
        }),
    )
}

fn flush_op(cid: u16, cqe_permit: CompQueueEntryPermit) -> Request {
    Request::new_flush(
        0,
        0, // TODO: is 0 enough or do we pass total size?
        Box::new(move |op, res, ctx| {
            complete_block_req(cid, op, res, cqe_permit, ctx)
        }),
    )
}

/// Callback invoked by the underlying Block Device once it has completed an I/O op.
///
/// Place the operation result (success or failure) onto the corresponding Completion Queue.
fn complete_block_req(
    cid: u16,
    op: Operation,
    res: block::Result,
    cqe_permit: CompQueueEntryPermit,
    ctx: &DispCtx,
) {
    let comp = match res {
        block::Result::Success => Completion::success(),
        block::Result::Failure => {
            Completion::generic_err(bits::STS_DATA_XFER_ERR)
        }
        block::Result::Unsupported => Completion::specific_err(
            bits::StatusCodeType::CmdSpecific,
            bits::STS_READ_CONFLICTING_ATTRS,
        ),
    };

    match op {
        Operation::Read(..) => {
            probes::nvme_read_complete!(|| (cid));
        }
        Operation::Write(..) => {
            probes::nvme_write_complete!(|| (cid));
        }
        _ => {}
    }

    cqe_permit.push_completion(cid, comp, ctx);
}
