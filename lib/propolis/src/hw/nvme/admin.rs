use std::cmp::min;
use std::mem::size_of;

use crate::common::{GuestAddr, GuestRegion, PAGE_SIZE};
use crate::vmm::MemCtx;

use super::bits::*;
use super::queue::{QueueId, ADMIN_QUEUE_ID};
use super::{cmds, NvmeCtrl, NvmeError, MAX_NUM_IO_QUEUES};

impl NvmeCtrl {
    /// Service Create I/O Completion Queue command.
    ///
    /// See NVMe 1.0e Section 5.3 Create I/O Completion Queue command
    pub(super) fn acmd_create_io_cq(
        &mut self,
        cmd: &cmds::CreateIOCQCmd,
        mem: &MemCtx,
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
            cmd.qid,
            cmd.intr_vector,
            GuestAddr(cmd.prp),
            cmd.qsize,
            mem,
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
        mem: &MemCtx,
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
            cmd.qid,
            cmd.cqid,
            GuestAddr(cmd.prp),
            cmd.qsize,
            mem,
        ) {
            Ok(_) => cmds::Completion::success(),
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
        match self.delete_cq(cqid) {
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
        match self.delete_sq(sqid) {
            Ok(()) => cmds::Completion::success(),
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

    /// Service Set Features command.
    ///
    /// See NVMe 1.0e Section 5.12 Set Features command
    pub(super) fn acmd_set_features(
        &self,
        cmd: &cmds::SetFeaturesCmd,
    ) -> cmds::Completion {
        match cmd.fid {
            cmds::FeatureIdent::NumberOfQueues { ncqr, nsqr } => {
                if ncqr == 0 || nsqr == 0 {
                    return cmds::Completion::generic_err(STS_INVAL_FIELD);
                }
                // TODO: error if called after initialization

                // If they ask for too many queues, just return our max possible
                let ncqa = min(ncqr as u32, MAX_NUM_IO_QUEUES as u32);
                let nsqa = min(nsqr as u32, MAX_NUM_IO_QUEUES as u32);

                // `ncqa`/`nsqa` are 0-based values so subtract 1
                cmds::Completion::success_val((ncqa - 1) << 16 | (nsqa - 1))
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
                cmds::Completion::generic_err(STS_INVAL_FIELD)
            }
        }
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
