use std::mem::size_of;

use super::bits::{self, *};
use crate::common::GuestAddr;
use crate::{common::PAGE_SIZE, dispatch::DispCtx};

use super::{cmds, NvmeCtrl, NvmeError};

impl NvmeCtrl {
    /// Service Create I/O Completion Queue command.
    ///
    /// See NVMe 1.0e Section 5.3 Create I/O Completion Queue command
    pub(super) fn acmd_create_io_cq(
        &mut self,
        cmd: &cmds::CreateIOCQCmd,
        ctx: &DispCtx,
    ) -> cmds::Completion {
        if cmd.intr_vector >= super::NVME_MSIX_COUNT {
            return cmds::Completion::specific_err(
                StatusCodeType::CmdSpecific,
                STS_CREATE_IO_Q_INVAL_INT_VEC,
            );
        }

        // We only support physical contiguous queues
        if !cmd.phys_contig {
            return cmds::Completion::generic_err(bits::STS_INVAL_FIELD);
        }

        // Finally, create the Completion Queue
        match self.create_cq(
            cmd.qid,
            cmd.intr_vector,
            GuestAddr(cmd.prp),
            cmd.qsize as u32,
            ctx,
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
        ctx: &DispCtx,
    ) -> cmds::Completion {
        // We only support physical contiguous queues
        if !cmd.phys_contig {
            return cmds::Completion::generic_err(bits::STS_INVAL_FIELD);
        }

        // Finally, create the Submission Queue
        match self.create_sq(
            cmd.qid,
            cmd.cqid,
            GuestAddr(cmd.prp),
            cmd.qsize as u32,
            ctx,
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

    /// Service Get Log Page command.
    ///
    /// See NVMe 1.0e Section 5.10 Get Log Page command
    pub(super) fn acmd_get_log_page(
        &self,
        cmd: &cmds::GetLogPageCmd,
        ctx: &DispCtx,
    ) -> cmds::Completion {
        assert!((cmd.len as usize) < PAGE_SIZE);
        let buf = cmd
            .data(ctx.mctx.memctx())
            .next()
            .expect("missing prp entry for log page response");
        // TODO: actually keep a log that we can write back instead of all zeros
        assert!(ctx.mctx.memctx().write_byte(buf.0, 0, cmd.len as usize));
        cmds::Completion::success()
    }

    /// Service Identify command.
    ///
    /// See NVMe 1.0e Section 5.11 Identify command
    pub(super) fn acmd_identify(
        &self,
        cmd: &cmds::IdentifyCmd,
        ctx: &DispCtx,
    ) -> cmds::Completion {
        match cmd.cns {
            IDENT_CNS_NAMESPACE => match cmd.nsid {
                // TODO: We only support a single namespace currently
                1 => {
                    assert!(size_of::<bits::IdentifyNamespace>() <= PAGE_SIZE);
                    let buf = cmd
                        .data(ctx.mctx.memctx())
                        .next()
                        .expect("missing prp entry for ident response");
                    assert!(ctx.mctx.memctx().write(buf.0, &self.ns.ident));
                    cmds::Completion::success()
                }
                // 0 is not a valid NSID (See NVMe 1.3, Section 6.1)
                // We also don't currently support namespace management
                // and so treat the 'broadcast' NSID (0xffffffff) as invalid
                // along with any other namespace
                0 | 0xffffffff | _ => {
                    cmds::Completion::generic_err(STS_INVALID_NS)
                }
            },
            IDENT_CNS_CONTROLLER => {
                let ident = bits::IdentifyController {
                    vid: self.vendor_id,
                    ssvid: self.vendor_id,
                    // TODO: fill out serial number
                    // TODO: move to const somewhere
                    ieee: [0xA8, 0x40, 0x25], // Oxide OUI
                    sqes: size_of::<bits::RawSubmission>() as u8,
                    cqes: size_of::<bits::RawCompletion>() as u8,
                    // hardcode a single namespace for now
                    nn: 1,
                    // bit 0 indicates volatile write cache is present
                    vwc: 1,
                    ..Default::default()
                };
                assert!(size_of::<bits::IdentifyController>() <= PAGE_SIZE);
                let buf = cmd
                    .data(ctx.mctx.memctx())
                    .next()
                    .expect("missing prp entry for ident response");
                assert!(ctx.mctx.memctx().write(buf.0, &ident));
                cmds::Completion::success()
            }
            // We currently present NVMe version 1.0 in which CNS is a 1-bit field
            // and hence only need to support the NAMESPACE and CONTROLLER cases
            _ => cmds::Completion::generic_err(bits::STS_INVAL_FIELD),
        }
    }

    /// Service Set Features command.
    ///
    /// See NVMe 1.0e Section 5.12 Set Features command
    pub(super) fn acmd_set_features(
        &self,
        cmd: &cmds::SetFeaturesCmd,
        _ctx: &DispCtx,
    ) -> cmds::Completion {
        if cmd.save {
            return cmds::Completion::specific_err(
                StatusCodeType::CmdSpecific,
                STS_SET_FEATURE_NOT_SAVEABLE,
            );
        }
        match cmd.fid {
            cmds::FeatureIdent::NumberOfQueues { ncqr, nsqr } => {
                // NVMe 1.3, Section 5.21.1.7: ncqr/nsqr can't be 65535
                if ncqr == 0xFFFF || nsqr == 0xFFFF {
                    return cmds::Completion::generic_err(STS_INVAL_FIELD);
                }
                // TODO: error if called after initialization
                // TODO: we only support a single pair of I/O queues
                let ncqa = 1;
                let nsqa = 1;
                // `ncqa`/`nsqa` are 0-based values so subtract 1
                cmds::Completion::success_val((ncqa - 1) << 16 | (nsqa - 1))
            }
            cmds::FeatureIdent::Reserved
            | cmds::FeatureIdent::Arbitration
            | cmds::FeatureIdent::PowerManagement
            | cmds::FeatureIdent::LbaRangeType
            | cmds::FeatureIdent::TemperatureThreshold
            | cmds::FeatureIdent::ErrorRecovery
            | cmds::FeatureIdent::VolatileWriteCache
            | cmds::FeatureIdent::InterruptCoalescing
            | cmds::FeatureIdent::InterruptVectorConfiguration
            | cmds::FeatureIdent::WriteAtomicityNormal
            | cmds::FeatureIdent::AsynchronousEventConfiguration
            | cmds::FeatureIdent::AutonomousPowerStateTransition
            | cmds::FeatureIdent::HostMemoryBuffer
            | cmds::FeatureIdent::Timestamp
            | cmds::FeatureIdent::KeepAliveTimer
            | cmds::FeatureIdent::HostControlledThermalManagement
            | cmds::FeatureIdent::NonOperationPowerStateConfig
            | cmds::FeatureIdent::Managment(_)
            | cmds::FeatureIdent::SoftwareProgressMarker
            | cmds::FeatureIdent::HostIdentifier
            | cmds::FeatureIdent::ReservationNotificationMask
            | cmds::FeatureIdent::ReservationPersistance
            | cmds::FeatureIdent::Vendor(_) => {
                cmds::Completion::generic_err(STS_INVAL_FIELD)
            }
        }
    }
}
