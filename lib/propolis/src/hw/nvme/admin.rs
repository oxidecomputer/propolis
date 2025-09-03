// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::mem::size_of;

use crate::common::{GuestAddr, GuestRegion, PAGE_SIZE};
use crate::vmm::MemCtx;

use super::bits::*;
use super::queue::{
    sqid_to_block_qid, DoorbellBuffer, QueueId, ADMIN_QUEUE_ID,
};
use super::{
    cmds, NvmeCtrl, NvmeError, PciNvme, MAX_NUM_IO_QUEUES, MAX_NUM_QUEUES,
};

#[usdt::provider(provider = "propolis")]
mod probes {
    fn nvme_abort(cid: u16, sqid: u16) {}
}

impl NvmeCtrl {
    /// Abort command.
    ///
    /// See NVMe 1.0e Section 5.1 Abort command
    pub(super) fn acmd_abort(&self, cmd: &cmds::AbortCmd) -> cmds::Completion {
        probes::nvme_abort!(|| (cmd.cid, cmd.sqid));

        // Verify the SQ in question currently exists
        let sqid = cmd.sqid as usize;
        if sqid >= MAX_NUM_QUEUES || self.sqs[sqid].is_none() {
            return cmds::Completion::generic_err(STS_INVAL_FIELD).dnr();
        }

        // TODO: Support aborting in-flight commands.

        // The NVMe spec does not make any guarantees about being able to
        // successfully abort commands and allows indicating a failure to
        // do so back to the host software. We do so here by returning a
        // "success" value with bit 0 set to '1'.
        cmds::Completion::success_val(1)
    }

    /// Service Create I/O Completion Queue command.
    ///
    /// See NVMe 1.0e Section 5.3 Create I/O Completion Queue command
    pub(super) fn acmd_create_io_cq(
        &mut self,
        cmd: &cmds::CreateIOCQCmd,
        nvme: &PciNvme,
    ) -> cmds::Completion {
        // If the host hasn't specified an IOCQES, fail this request
        if self.ctrl.cc.iocqes() == 0 {
            return cmds::Completion::specific_err(
                StatusCodeType::CmdSpecific,
                STS_CREATE_IO_Q_INVAL_QSIZE,
            );
        }

        if cmd.intr_vector >= super::NVME_MSIX_COUNT {
            return cmds::Completion::specific_err(
                StatusCodeType::CmdSpecific,
                STS_CREATE_IO_Q_INVAL_INT_VEC,
            );
        }

        // We only support physical contiguous queues
        if !cmd.phys_contig {
            return cmds::Completion::generic_err(STS_INVAL_FIELD);
        }

        // Finally, create the Completion Queue
        match self.create_cq(
            super::queue::CreateParams {
                id: cmd.qid,
                base: GuestAddr(cmd.prp),
                size: cmd.qsize,
            },
            false,
            cmd.intr_vector,
            nvme,
        ) {
            Ok(_) => cmds::Completion::success(),
            Err(
                NvmeError::InvalidCompQueue(_)
                | NvmeError::CompQueueAlreadyExists(_),
            ) => cmds::Completion::specific_err(
                StatusCodeType::CmdSpecific,
                STS_CREATE_IO_Q_INVAL_QID,
            ),
            Err(NvmeError::QueueCreateErr(err)) => err.into(),
            Err(_) => cmds::Completion::generic_err(STS_INTERNAL_ERR),
        }
    }

    /// Service I/O Create Submission Queue command.
    ///
    /// See NVMe 1.0e Section 5.4 Create I/O Submission Queue command
    pub(super) fn acmd_create_io_sq(
        &mut self,
        cmd: &cmds::CreateIOSQCmd,
        nvme: &PciNvme,
    ) -> cmds::Completion {
        // If the host hasn't specified an IOSQES, fail this request
        if self.ctrl.cc.iosqes() == 0 {
            return cmds::Completion::specific_err(
                StatusCodeType::CmdSpecific,
                STS_CREATE_IO_Q_INVAL_QSIZE,
            );
        }

        // We only support physical contiguous queues
        if !cmd.phys_contig {
            return cmds::Completion::generic_err(STS_INVAL_FIELD);
        }

        // Finally, create the Submission Queue
        match self.create_sq(
            super::queue::CreateParams {
                id: cmd.qid,
                base: GuestAddr(cmd.prp),
                size: cmd.qsize,
            },
            false,
            cmd.cqid,
            nvme,
        ) {
            Ok(sq) => {
                self.io_sq_post_create(nvme, sq);
                cmds::Completion::success()
            }
            Err(NvmeError::InvalidCompQueue(_)) => {
                cmds::Completion::specific_err(
                    StatusCodeType::CmdSpecific,
                    STS_CREATE_IO_Q_INVAL_CQ,
                )
            }
            Err(
                NvmeError::InvalidSubQueue(_)
                | NvmeError::SubQueueAlreadyExists(_),
            ) => cmds::Completion::specific_err(
                StatusCodeType::CmdSpecific,
                STS_CREATE_IO_Q_INVAL_QID,
            ),
            Err(NvmeError::QueueCreateErr(err)) => err.into(),
            Err(_) => cmds::Completion::generic_err(STS_INTERNAL_ERR),
        }
    }

    /// Service I/O Delete Completion Queue command.
    ///
    /// See NVMe 1.0e Section 5.5 Delete I/O Submission Queue command
    pub(super) fn acmd_delete_io_cq(
        &mut self,
        cqid: QueueId,
        nvme: &PciNvme,
    ) -> cmds::Completion {
        // Not allowed to delete the Admin Completion Queue
        if cqid == ADMIN_QUEUE_ID {
            return cmds::Completion::specific_err(
                StatusCodeType::CmdSpecific,
                STS_DELETE_IO_Q_INVAL_QID,
            );
        }

        // Remove the CQ from our list of active CQs.
        // At this point, all associated SQs should've been deleted
        // otherwise we'll return an error.
        match self.delete_cq(cqid, nvme) {
            Ok(()) => cmds::Completion::success(),
            Err(NvmeError::InvalidCompQueue(_)) => {
                cmds::Completion::specific_err(
                    StatusCodeType::CmdSpecific,
                    STS_DELETE_IO_Q_INVAL_QID,
                )
            }
            Err(NvmeError::AssociatedSubQueuesStillExist(_, _)) => {
                cmds::Completion::specific_err(
                    StatusCodeType::CmdSpecific,
                    STS_DELETE_IO_Q_INVAL_Q_DELETION,
                )
            }
            _ => cmds::Completion::generic_err(STS_INTERNAL_ERR),
        }
    }

    /// Service I/O Delete Submission Queue command.
    ///
    /// See NVMe 1.0e Section 5.6 Delete I/O Submission Queue command
    pub(super) fn acmd_delete_io_sq(
        &mut self,
        sqid: QueueId,
        nvme: &PciNvme,
    ) -> cmds::Completion {
        // Not allowed to delete the Admin Submission Queue
        if sqid == ADMIN_QUEUE_ID {
            return cmds::Completion::specific_err(
                StatusCodeType::CmdSpecific,
                STS_DELETE_IO_Q_INVAL_QID,
            );
        }

        // Remove the SQ from our list of active SQs which will stop
        // us from accepting any new requests for it.
        // That should be the only strong ref left to the SubQueue
        // Any in-flight I/O requests that haven't been completed yet
        // only hold a weak ref (via CompQueueEntryPermit).
        // Note: The NVMe 1.0e spec says "The command causes all commands
        //       submitted to the indicated Submission Queue that are still in
        //       progress to be aborted."
        match self.delete_sq(sqid, nvme) {
            Ok(()) => {
                nvme.block_attach.queue_dissociate(sqid_to_block_qid(sqid));
                // TODO: wait until requests are done?
                cmds::Completion::success()
            }
            Err(NvmeError::InvalidSubQueue(_)) => {
                cmds::Completion::specific_err(
                    StatusCodeType::CmdSpecific,
                    STS_DELETE_IO_Q_INVAL_QID,
                )
            }
            _ => cmds::Completion::generic_err(STS_INTERNAL_ERR),
        }
    }

    /// Service Get Log Page command.
    ///
    /// See NVMe 1.0e Section 5.10 Get Log Page command
    pub(super) fn acmd_get_log_page(
        &self,
        cmd: &cmds::GetLogPageCmd,
        mem: &MemCtx,
    ) -> cmds::Completion {
        if let Some(regions) = cmd
            .data(mem)
            .map(|r| mem.writable_region(&r))
            .collect::<Option<Vec<_>>>()
        {
            // TODO: Keep a log to write back instead of 0s
            for region in regions {
                let _ = region.write_byte(0, region.len());
            }
            cmds::Completion::success()
        } else {
            cmds::Completion::generic_err(STS_DATA_XFER_ERR)
        }
    }

    /// Service Identify command.
    ///
    /// See NVMe 1.0e Section 5.11 Identify command
    pub(super) fn acmd_identify(
        &self,
        cmd: &cmds::IdentifyCmd,
        mem: &MemCtx,
    ) -> cmds::Completion {
        match cmd.cns {
            IDENT_CNS_NAMESPACE => match cmd.nsid {
                1 => {
                    assert!(size_of::<IdentifyNamespace>() <= PAGE_SIZE);
                    match Self::write_admin_result(
                        cmd.data(mem),
                        &self.ns_ident,
                        mem,
                    ) {
                        Some(_) => cmds::Completion::success(),
                        None => {
                            cmds::Completion::generic_err(STS_DATA_XFER_ERR)
                        }
                    }
                }
                // 0 is not a valid NSID (See NVMe 1.0e, Section 6.1 Namespaces)
                // We also don't currently support namespace management
                // and so treat the 'broadcast' NSID (0xffffffff) as invalid
                // along with any other namespace
                0 | 0xffffffff => cmds::Completion::generic_err(STS_INVALID_NS),
                _ => cmds::Completion::generic_err(STS_INVALID_NS),
            },
            IDENT_CNS_CONTROLLER => {
                assert!(size_of::<IdentifyController>() <= PAGE_SIZE);

                match Self::write_admin_result(
                    cmd.data(mem),
                    &self.ctrl_ident,
                    mem,
                ) {
                    Some(_) => cmds::Completion::success(),
                    None => cmds::Completion::generic_err(STS_DATA_XFER_ERR),
                }
            }
            // We currently present NVMe version 1.0 in which CNS is a 1-bit field
            // and hence only need to support the NAMESPACE and CONTROLLER cases
            _ => cmds::Completion::generic_err(STS_INVAL_FIELD),
        }
    }

    /// Service Get Features command.
    ///
    /// See NVMe 1.0e Section 5.9 Get Features command
    pub(super) fn acmd_get_features(
        &self,
        cmd: &cmds::GetFeaturesCmd,
    ) -> cmds::Completion {
        match cmd.fid {
            // Mandatory features
            cmds::FeatureIdent::Arbitration => {
                // no-limit for arbitration burst, all other fields zeroed
                let val = 0b111;
                cmds::Completion::success_val(val)
            }
            cmds::FeatureIdent::PowerManagement => {
                // Empty value with unspecified workload hint
                cmds::Completion::success_val(0)
            }
            cmds::FeatureIdent::TemperatureThreshold => {
                let query = cmds::FeatTemperatureThreshold(cmd.cdw11);

                use cmds::{
                    ThresholdTemperatureSelect as TempSel,
                    ThresholdTypeSelect as TypeSel,
                };
                match (query.tmpsel(), query.thsel()) {
                    (TempSel::Reserved(_), _) | (_, TypeSel::Reserved(_)) => {
                        // squawk about reserved bits being set
                        cmds::Completion::generic_err(STS_INVAL_FIELD)
                    }
                    (TempSel::Composite, typesel) => {
                        const KELVIN_0C: u16 = 273;
                        let mut out = cmds::FeatTemperatureThreshold(0);
                        out.set_tmpsel(TempSel::Composite);
                        out.set_thsel(typesel);
                        match typesel {
                            TypeSel::Over => out.set_tmpth(KELVIN_0C + 100),
                            TypeSel::Under => out.set_tmpth(0),
                            TypeSel::Reserved(_) => unreachable!(),
                        }
                        cmds::Completion::success_val(out.0)
                    }
                    (tempsel, typesel) => {
                        let mut out = cmds::FeatTemperatureThreshold(0);
                        out.set_tmpsel(tempsel);
                        out.set_thsel(typesel);
                        match typesel {
                            TypeSel::Over => out.set_tmpth(0xffff),
                            TypeSel::Under => out.set_tmpth(0),
                            TypeSel::Reserved(_) => unreachable!(),
                        }
                        cmds::Completion::success_val(out.0)
                    }
                }
            }
            cmds::FeatureIdent::ErrorRecovery => {
                // Empty value indicating we do none of this
                cmds::Completion::success_val(0)
            }
            cmds::FeatureIdent::NumberOfQueues => {
                // Until we track the maximums set by the guest, just report the
                // maximums supported
                cmds::Completion::success_val(
                    cmds::FeatNumberQueues {
                        ncq: MAX_NUM_IO_QUEUES as u16,
                        nsq: MAX_NUM_IO_QUEUES as u16,
                    }
                    .into(),
                )
            }
            cmds::FeatureIdent::InterruptCoalescing => {
                // A value of 0 indicates no configured coalescing
                cmds::Completion::success_val(0)
            }
            cmds::FeatureIdent::InterruptVectorConfiguration => {
                let cfg: cmds::FeatInterruptVectorConfig = cmd.cdw11.into();

                // report disabled coalescing for all vectors
                cmds::Completion::success_val(
                    cmds::FeatInterruptVectorConfig { iv: cfg.iv, cd: true }
                        .into(),
                )
            }
            cmds::FeatureIdent::WriteAtomicity => {
                // Value of 0 indicates no Disable Normal setting
                cmds::Completion::success_val(0)
            }
            cmds::FeatureIdent::AsynchronousEventConfiguration => {
                // None of the defined events result in AEN transmission
                cmds::Completion::success_val(0)
            }

            // Optional features
            cmds::FeatureIdent::VolatileWriteCache => {
                // TODO: wire into actual write cache state
                //
                // Until that is done, indicate an enabled write cache to ensure
                // IO flushes for the backends which require it for consistency.
                cmds::Completion::success_val(
                    cmds::FeatVolatileWriteCache { wce: true }.into(),
                )
            }

            cmds::FeatureIdent::Reserved
            | cmds::FeatureIdent::LbaRangeType
            | cmds::FeatureIdent::SoftwareProgressMarker
            | cmds::FeatureIdent::Vendor(_) => {
                cmds::Completion::generic_err(STS_INVAL_FIELD).dnr()
            }
        }
    }

    /// Service Set Features command.
    ///
    /// See NVMe 1.0e Section 5.12 Set Features command
    pub(super) fn acmd_set_features(
        &self,
        cmd: &cmds::SetFeaturesCmd,
    ) -> cmds::Completion {
        match cmd.fid {
            cmds::FeatureIdent::NumberOfQueues => {
                let nq: cmds::FeatNumberQueues = match cmd.cdw11.try_into() {
                    Ok(f) => f,
                    Err(_) => {
                        return cmds::Completion::generic_err(STS_INVAL_FIELD);
                    }
                };

                // TODO: error if called after initialization

                // If they ask for too many queues, just return our max possible
                let clamped = cmds::FeatNumberQueues {
                    ncq: nq.ncq.min(MAX_NUM_IO_QUEUES as u16),
                    nsq: nq.nsq.min(MAX_NUM_IO_QUEUES as u16),
                };

                cmds::Completion::success_val(clamped.into())
            }
            cmds::FeatureIdent::VolatileWriteCache => {
                // NVMe 1.0e Figure 66 Identify - Identify Controller Data
                // Structure "If a volatile write cache [VWC] is present, then
                // the host may ... control whether it is enabled with Set
                // Features specifying the Volatile Write Cache feature
                // identifier."
                cmds::Completion::success()
            }
            cmds::FeatureIdent::Reserved
            | cmds::FeatureIdent::Arbitration
            | cmds::FeatureIdent::PowerManagement
            | cmds::FeatureIdent::LbaRangeType
            | cmds::FeatureIdent::TemperatureThreshold
            | cmds::FeatureIdent::ErrorRecovery
            | cmds::FeatureIdent::InterruptCoalescing
            | cmds::FeatureIdent::InterruptVectorConfiguration
            | cmds::FeatureIdent::WriteAtomicity
            | cmds::FeatureIdent::AsynchronousEventConfiguration
            | cmds::FeatureIdent::SoftwareProgressMarker
            | cmds::FeatureIdent::Vendor(_) => {
                cmds::Completion::generic_err(STS_INVAL_FIELD).dnr()
            }
        }
    }

    pub(super) fn acmd_doorbell_buf_cfg(
        &mut self,
        cmd: &cmds::DoorbellBufCfgCmd,
    ) -> cmds::Completion {
        let mps_mask = self.get_mps() - 1;

        if cmd.shadow_doorbell_buffer & mps_mask != 0
            || cmd.eventidx_buffer & mps_mask != 0
        {
            return cmds::Completion::generic_err(STS_INVAL_FIELD);
        }

        let db_buf = DoorbellBuffer {
            shadow: GuestAddr(cmd.shadow_doorbell_buffer),
            eventidx: GuestAddr(cmd.eventidx_buffer),
        };

        self.doorbell_buf = Some(db_buf);

        for cq in self.cqs.iter().flatten() {
            cq.set_db_buf(Some(db_buf), false);
        }
        for sq in self.sqs.iter().flatten() {
            sq.set_db_buf(Some(db_buf), false);
        }

        cmds::Completion::success()
    }

    /// Write result data from an admin command into host memory
    ///
    /// The `data` type must be `repr(packed(1))`
    ///
    /// Returns `Some(())` if successful, else None
    fn write_admin_result<T: Copy>(
        prp: cmds::PrpIter,
        data: &T,
        mem: &MemCtx,
    ) -> Option<()> {
        let bufs: Vec<GuestRegion> = prp.collect();
        if size_of::<T>() > bufs.iter().map(|r| r.1).sum::<usize>() {
            // Not enough space
            return None;
        }
        let regions = bufs
            .into_iter()
            .map(|r| mem.writable_region(&r))
            .collect::<Option<Vec<_>>>()?;
        if regions.len() == 1 {
            // Can be copied to one contiguous page
            regions[0].write(data).ok()?;
            Some(())
        } else {
            // Split across multiple pages

            // Safety:
            //
            // We expect and demand that the resulting structs written through
            // this function are packed, such that there is no padding to risk
            // UB through the [u8] slice creation.
            let mut raw = unsafe {
                std::slice::from_raw_parts(
                    data as *const T as *const u8,
                    size_of::<T>(),
                )
            };
            let mut copied = 0;
            for region in regions {
                let write_len = usize::min(region.len(), raw.len());

                let to_copy;
                (to_copy, raw) = raw.split_at(write_len);
                copied += region.write_bytes(&to_copy).ok()?;

                if raw.is_empty() {
                    break;
                }
            }
            assert_eq!(copied, size_of::<T>());
            Some(())
        }
    }
}
