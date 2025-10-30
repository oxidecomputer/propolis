// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use super::bits::{self, StatusCodeType, SubmissionQueueEntry};
use super::queue::{QueueCreateErr, QueueId};
use crate::block;
use crate::common::*;
use crate::vmm::MemCtx;

use bitstruct::bitstruct;
use thiserror::Error;

#[usdt::provider(provider = "propolis")]
mod probes {
    fn nvme_prp_entry(iter: u64, prp: u64) {}
    fn nvme_prp_list(iter: u64, prp: u64, idx: u16) {}
    fn nvme_prp_error(err: &'static str) {}
}

/// Errors that may be encountered during command parsing.
#[derive(Debug, Error)]
pub enum ParseErr {
    /// We do not currently support fused operations
    #[error("Fused ops not supported")]
    Fused,

    /// An invalid value was specified in the FUSE bits of `CDW0`.
    #[error("reserved FUSE value specified")]
    ReservedFuse,
}

/// A parsed Admin Command
#[derive(Debug)]
pub enum AdminCmd {
    /// Delete the specified I/O Submission Queue
    DeleteIOSubQ(QueueId),
    /// Create the specified I/O Submission Queue
    CreateIOSubQ(CreateIOSQCmd),
    /// Get Log Page Command
    GetLogPage(GetLogPageCmd),
    /// Delete the specified I/O Completion Queue
    DeleteIOCompQ(QueueId),
    /// Create the specified I/O Completion Queue
    CreateIOCompQ(CreateIOCQCmd),
    /// Identify Command
    Identify(IdentifyCmd),
    /// Abort Command
    Abort(AbortCmd),
    /// Set Features Command
    SetFeatures(SetFeaturesCmd),
    /// Get Features Command
    GetFeatures(GetFeaturesCmd),
    /// Asynchronous Event Request Command
    AsyncEventReq,
    /// Doorbell Buffer Config Command
    DoorbellBufCfg(DoorbellBufCfgCmd),
    /// An unknown admin command
    Unknown(#[allow(dead_code)] GuestData<SubmissionQueueEntry>),
}

impl AdminCmd {
    /// Try to parse an `AdminCmd` out of a raw Submission Entry.
    pub fn parse(
        raw: GuestData<SubmissionQueueEntry>,
    ) -> Result<Self, ParseErr> {
        let cmd = match raw.opcode() {
            bits::ADMIN_OPC_DELETE_IO_SQ => {
                AdminCmd::DeleteIOSubQ(raw.cdw10 as u16)
            }
            bits::ADMIN_OPC_CREATE_IO_SQ => {
                let queue_prio = match (raw.cdw11 & 0b110) >> 1 {
                    0b00 => QueuePriority::Urgent,
                    0b01 => QueuePriority::High,
                    0b10 => QueuePriority::Medium,
                    0b11 => QueuePriority::Low,
                    _ => unreachable!(),
                };
                AdminCmd::CreateIOSubQ(CreateIOSQCmd {
                    prp: raw.prp1,
                    qsize: (raw.cdw10 >> 16) + 1, // Convert from 0's based
                    qid: raw.cdw10 as u16,
                    cqid: (raw.cdw11 >> 16) as u16,
                    queue_prio,
                    phys_contig: (raw.cdw11 & 1) != 0,
                })
            }
            bits::ADMIN_OPC_GET_LOG_PAGE => {
                AdminCmd::GetLogPage(GetLogPageCmd {
                    nsid: raw.nsid,
                    // Convert from 0's based dword
                    len: (((raw.cdw10 & 0xFFF) >> 16) + 1) * 4,
                    log_page_ident: LogPageIdent::from(raw.cdw10 as u8),
                    prp1: raw.prp1,
                    prp2: raw.prp2,
                })
            }
            bits::ADMIN_OPC_DELETE_IO_CQ => {
                AdminCmd::DeleteIOCompQ(raw.cdw10 as u16)
            }
            bits::ADMIN_OPC_CREATE_IO_CQ => {
                AdminCmd::CreateIOCompQ(CreateIOCQCmd {
                    prp: raw.prp1,
                    qsize: (raw.cdw10 >> 16) + 1, // Convert from 0's based
                    qid: raw.cdw10 as u16,
                    intr_vector: (raw.cdw11 >> 16) as u16,
                    intr_enable: (raw.cdw11 & 0b10) != 0,
                    phys_contig: (raw.cdw11 & 0b1) != 0,
                })
            }
            bits::ADMIN_OPC_IDENTIFY => AdminCmd::Identify(IdentifyCmd {
                // Only the last bit is used for NVMe 1.0e
                cns: raw.cdw10 as u8 & 0b1,
                nsid: raw.nsid,
                prp1: raw.prp1,
                prp2: raw.prp2,
            }),
            bits::ADMIN_OPC_ABORT => AdminCmd::Abort(AbortCmd {
                cid: (raw.cdw10 >> 16) as u16,
                sqid: raw.cdw10 as u16,
            }),
            bits::ADMIN_OPC_SET_FEATURES => {
                AdminCmd::SetFeatures(SetFeaturesCmd {
                    fid: FeatureIdent::from(raw.cdw10 as u8),
                    cdw11: raw.cdw11,
                })
            }
            bits::ADMIN_OPC_GET_FEATURES => {
                AdminCmd::GetFeatures(GetFeaturesCmd {
                    fid: FeatureIdent::from(raw.cdw10 as u8),
                    cdw11: raw.cdw11,
                })
            }
            bits::ADMIN_OPC_ASYNC_EVENT_REQ => AdminCmd::AsyncEventReq,
            bits::ADMIN_OPC_DOORBELL_BUF_CFG => {
                AdminCmd::DoorbellBufCfg(DoorbellBufCfgCmd {
                    shadow_doorbell_buffer: raw.prp1,
                    eventidx_buffer: raw.prp2,
                })
            }
            _ => AdminCmd::Unknown(raw),
        };
        let _fuse = match (raw.cdw0 >> 8) & 0b11 {
            0b00 => Ok(()),               // Normal (non-fused) operation
            0b01 => Err(ParseErr::Fused), // First fused op
            0b10 => Err(ParseErr::Fused), // Second fused op
            _ => Err(ParseErr::ReservedFuse),
        }?;
        Ok(cmd)
    }
}

/// Create I/O Completion Queue Command Parameters
#[allow(dead_code)]
#[derive(Debug)]
pub struct CreateIOCQCmd {
    /// PRP Entry 1 (PRP1)
    ///
    /// If the queue is physically contiguous, then this is the 64-bit base address
    /// of the Completion Queue in guest memory. Otherwise it a PRP List pointer of
    /// the pages that constitute the Completion Queue.
    pub prp: u64,

    /// Queue Size (QSIZE)
    ///
    /// The size of the Completion Queue to be created.
    /// See NVMe 1.0e Section 4.1.3 Queue Size
    /// NOTE: This has already been converted from a 0's based value.
    pub qsize: u32,

    /// Queue Identifier (QID)
    ///
    /// The identifier to assign to the Completion Queue to be created.
    /// See NVMe 1.0e Section 4.1.4 Queue Identifier
    pub qid: QueueId,

    /// Interrupt Vector (IV)
    ///
    /// The Interrupt Vector used to signal to the host (VM) upon pushing
    /// entries onto the Completion Queue.
    pub intr_vector: u16,

    /// Interrupts Enabled (IEN)
    ///
    /// Whether or not interrupts are enabled for this Completion Queue.
    pub intr_enable: bool,

    /// Physically Contiguous (PC)
    ///
    /// Whether or not the Completion Queue is physically contiguous in memory.
    pub phys_contig: bool,
}

/// Create I/O Submission Queue Command Parameters
#[allow(dead_code)]
#[derive(Debug)]
pub struct CreateIOSQCmd {
    /// PRP Entry 1 (PRP1)
    ///
    /// If the queue is physically contiguous, then this is the 64-bit base address
    /// of the Submission Queue in guest memory. Otherwise it a PRP List pointer of
    /// the pages that constitute the Completion Queue.
    pub prp: u64,

    /// Queue Size (QSIZE)
    ///
    /// The size of the Completion Queue to be created.
    /// See NVMe 1.0e Section 4.1.3 Queue Size
    /// NOTE: This has already been converted from a 0's based value.
    pub qsize: u32,

    /// Queue Identifier (QID)
    ///
    /// The identifier to assign to the Completion Queue to be created.
    /// See NVMe 1.0e Section 4.1.4 Queue Identifier
    pub qid: QueueId,

    /// Completion Queue Identifier (CQID)
    ///
    /// The ID of the corresponding Completion Queue for this Submission Queue.
    pub cqid: QueueId,

    /// Queue Priority (QPRIO)
    ///
    /// The priority service class for commands within this Submission Queue.
    /// Only used when the weighted round robin with an urgent priority service
    /// class is the arbitration mechanism selected.
    /// See NVMe 1.0e Section 4.7 Command Arbitration
    pub queue_prio: QueuePriority,

    /// Physically Contiguous (PC)
    ///
    /// Whether or not the Submission Queue is physically contiguous in memory.
    pub phys_contig: bool,
}

/// Priority Levels
#[derive(Debug)]
pub enum QueuePriority {
    /// Highest strict priority class (excluding Commands submitted to Admin Submission Queue)
    Urgent,
    /// Lowest strict priority class: Level - High
    High,
    /// Lowest strict priority class: Level - Medium
    Medium,
    /// Lowest strict priority class: Level - Low
    Low,
}

/// Get Log Page Command Parameters
#[allow(dead_code)]
#[derive(Debug)]
pub struct GetLogPageCmd {
    /// Namespace Identifier (NSID)
    ///
    /// The namespace that this command applies to.
    pub nsid: u32,

    /// The number of bytes to return.
    pub len: u32,

    /// Log Page Identifier (LID)
    ///
    /// The ID of the log page to retrieve.
    pub log_page_ident: LogPageIdent,

    /// PRP Entry 1 (PRP1)
    ///
    /// The first PRP entry specifying the start of the data buffer.
    prp1: u64,

    /// PRP Entry 2 (PRP2)
    ///
    /// If PRP1 specifies enough space, then PRP2 is reserved. Otherwise
    /// PRP2 specifies the second page and remainder of the data. It may
    /// not be a PRP List.
    prp2: u64,
}

impl GetLogPageCmd {
    /// Returns an Iterator that yields [`GuestRegion`]'s to write the log page
    /// data to.
    ///
    /// The expected size of the memory covered by the PRPs is defined by
    /// `NUMD`, stored as bytes (rather than number Dwords) in [`Self::len`]
    pub fn data<'a>(&self, mem: &'a MemCtx) -> PrpIter<'a> {
        PrpIter::new(u64::from(self.len), self.prp1, self.prp2, mem)
    }
}

/// The type of Log pages that may be retrieved with the Get Log Page command.
///
/// See NVMe 1.0e Section 5.10.1, Figure 58 Get Log Page - Log Page Identifiers
#[allow(dead_code)]
#[derive(Debug)]
pub enum LogPageIdent {
    /// Reserved Log Page
    Reserved,
    /// Error Information Log Page
    Error,
    /// SMART / Health Information Log Page
    Smart,
    /// Firmware Slot Information Log PAge
    Firmware,
    /// I/O Command Set Specific Log Page
    IOSpecifc(u8),
    /// Vendor Specific Log Page
    Vendor(u8),
}

impl From<u8> for LogPageIdent {
    fn from(ident: u8) -> Self {
        match ident {
            0 => LogPageIdent::Reserved,
            1 => LogPageIdent::Error,
            2 => LogPageIdent::Smart,
            3 => LogPageIdent::Firmware,
            0x04..=0x7F => LogPageIdent::Reserved,
            0x80..=0xBF => LogPageIdent::IOSpecifc(ident),
            0xC0..=0xFF => LogPageIdent::Vendor(ident),
        }
    }
}

/// Identify Command Parameters
#[derive(Debug)]
pub struct IdentifyCmd {
    /// The type of Identify data structure to return
    pub cns: u8,

    /// Namespace Identifier (NSID)
    ///
    /// The namespace that this command applies to.
    pub nsid: u32,

    /// PRP Entry 1 (PRP1)
    ///
    /// The first PRP entry specifying the start of the data buffer.
    prp1: u64,

    /// PRP Entry 2 (PRP2)
    ///
    /// If PRP1 specifies enough space, then PRP2 is reserved. Otherwise
    /// PRP2 specifies the second page and remainder of the data. It may
    /// not be a PRP List.
    prp2: u64,
}

impl IdentifyCmd {
    /// Returns an Iterator that yields [`GuestRegion`]'s to write the identify structure data to.
    pub fn data<'a>(&self, mem: &'a MemCtx) -> PrpIter<'a> {
        PrpIter::new(PAGE_SIZE as u64, self.prp1, self.prp2, mem)
    }
}

/// Abort Command Parameters
#[derive(Debug)]
pub struct AbortCmd {
    /// The command identifier of the command to be aborted.
    pub cid: u16,

    /// The ID of the Submission Queue asssociated with the command to be
    /// aborted.
    pub sqid: u16,
}

/// Get Features Command Parameters
#[derive(Debug)]
pub struct GetFeaturesCmd {
    /// Feature Identifier (FID)
    ///
    /// The feature that attributes are being specified for.
    pub fid: FeatureIdent,

    pub cdw11: u32,
}

/// Set Features Command Parameters
#[derive(Debug)]
pub struct SetFeaturesCmd {
    /// Feature Identifier (FID)
    ///
    /// The feature that attributes are being specified for.
    pub fid: FeatureIdent,

    pub cdw11: u32,
}

/// Doorbell Buffer Config Comannd Parameters
#[derive(Debug)]
pub struct DoorbellBufCfgCmd {
    pub shadow_doorbell_buffer: u64,
    pub eventidx_buffer: u64,
}

/// Feature Identifiers
///
/// See NVMe 1.0e Section 5.12.1, Figure 73 Set Features - Feature Identifiers
/// TODO: Fill out parameters for rest of variants
#[derive(Debug)]
pub enum FeatureIdent {
    /// Reserved or unknown feature identifier
    Reserved,
    /// Arbitration
    ///
    /// Controls command arbitration.
    /// See NVMe 1.0e Section 4.7 Command Arbitration
    Arbitration,
    /// Power Management
    ///
    /// Allows configuring power state.
    PowerManagement,
    /// LBA Range Type
    ///
    /// Indicates the type and attributes of LBA ranges that part of the specified namespace.
    LbaRangeType,
    /// Temperature Threshold
    ///
    /// Indicates the threshold for the temperature of the overall device (controller and NVM) in Kelvin.
    TemperatureThreshold,
    /// Error Recovery
    ///
    /// Controls error recovery attributes.
    ErrorRecovery,
    /// Volatile Write Cache
    ///
    /// Control the volatile write cache, if present.
    VolatileWriteCache,
    /// Number of Queues
    ///
    /// Indicates the number of queues requested to the controller.
    /// Only allowed during initialization and cannot change between resets.
    NumberOfQueues,
    /// Interrupt Coalescing
    ///
    /// Allows configuring interrupt coalescing settings.
    InterruptCoalescing,
    /// Interrupt Vector Configuration
    ///
    /// Allows confuring settings specific to a particular interrupt vector.
    InterruptVectorConfiguration,
    /// Write Atomicity
    ///
    /// Control write atomicity.
    WriteAtomicity,
    /// Asynchronous Event Configuration
    ///
    /// Controls the events that trigger an asynchronous event notification.
    AsynchronousEventConfiguration,
    /// Software Progress Marker
    ///
    /// This feature is persistent across power states.
    /// See NVMe 1.0e Section 7.6.1.1 Software Progress Marker
    SoftwareProgressMarker,
    /// Vendor specific feature.
    Vendor(#[allow(dead_code)] u8),
}

impl From<u8> for FeatureIdent {
    fn from(fid: u8) -> Self {
        use super::bits::*;
        use FeatureIdent::*;
        match fid {
            0 => Reserved,
            FEAT_ID_ARBITRATION => Arbitration,
            FEAT_ID_POWER_MGMT => PowerManagement,
            FEAT_ID_LBA_RANGE_TYPE => LbaRangeType,
            FEAT_ID_TEMP_THRESH => TemperatureThreshold,
            FEAT_ID_ERROR_RECOVERY => ErrorRecovery,
            FEAT_ID_VOLATILE_WRITE_CACHE => VolatileWriteCache,
            FEAT_ID_NUM_QUEUES => NumberOfQueues,
            FEAT_ID_INTR_COALESCE => InterruptCoalescing,
            FEAT_ID_INTR_VEC_CFG => InterruptVectorConfiguration,
            FEAT_ID_WRITE_ATOMIC => WriteAtomicity,
            FEAT_ID_ASYNC_EVENT_CFG => AsynchronousEventConfiguration,
            0xC..=0x7F => Reserved,
            0x80 => SoftwareProgressMarker,
            0x81..=0xBF => Reserved,
            0xC0..=0xFF => Vendor(fid),
        }
    }
}

bitstruct! {
    #[derive(Clone, Copy, Default)]
    pub struct FeatTemperatureThreshold(pub u32) {
        /// Temperature Threshold (TMPTH)
        pub tmpth: u16 = 0..16;

        /// Threshold Temperature Select (TMPSEL)
        pub tmpsel: ThresholdTemperatureSelect = 16..20;

        /// Threshold Type Select (THSEL)
        pub thsel: ThresholdTypeSelect = 20..22;

        /// Reserved
        reserved: u32 = 22..32;
    }
}

#[derive(Copy, Clone, PartialEq, Eq)]
pub enum ThresholdTemperatureSelect {
    Composite,
    Sensor1,
    Sensor2,
    Sensor3,
    Sensor4,
    Sensor5,
    Sensor6,
    Sensor7,
    Sensor8,
    Reserved(u8),
    All,
}
impl bitstruct::FromRaw<u8, ThresholdTemperatureSelect>
    for FeatTemperatureThreshold
{
    fn from_raw(raw: u8) -> ThresholdTemperatureSelect {
        use ThresholdTemperatureSelect::*;
        match raw {
            0b0000 => Composite,
            0b0001 => Sensor1,
            0b0010 => Sensor2,
            0b0011 => Sensor3,
            0b0100 => Sensor4,
            0b0101 => Sensor5,
            0b0110 => Sensor6,
            0b0111 => Sensor7,
            0b1000 => Sensor8,
            0b1111 => All,
            val => Reserved(val),
        }
    }
}
impl bitstruct::IntoRaw<u8, ThresholdTemperatureSelect>
    for FeatTemperatureThreshold
{
    fn into_raw(target: ThresholdTemperatureSelect) -> u8 {
        use ThresholdTemperatureSelect::*;
        match target {
            Composite => 0b0000,
            Sensor1 => 0b0001,
            Sensor2 => 0b0010,
            Sensor3 => 0b0011,
            Sensor4 => 0b0100,
            Sensor5 => 0b0101,
            Sensor6 => 0b0110,
            Sensor7 => 0b0111,
            Sensor8 => 0b1000,
            All => 0b1111,
            Reserved(val) => val,
        }
    }
}

#[derive(Copy, Clone, PartialEq, Eq)]
pub enum ThresholdTypeSelect {
    Over,
    Under,
    Reserved(u8),
}
impl bitstruct::FromRaw<u8, ThresholdTypeSelect> for FeatTemperatureThreshold {
    fn from_raw(raw: u8) -> ThresholdTypeSelect {
        use ThresholdTypeSelect::*;
        match raw {
            0b00 => Over,
            0b01 => Under,
            val => Reserved(val),
        }
    }
}
impl bitstruct::IntoRaw<u8, ThresholdTypeSelect> for FeatTemperatureThreshold {
    fn into_raw(target: ThresholdTypeSelect) -> u8 {
        use ThresholdTypeSelect::*;
        match target {
            Over => 0b00,
            Under => 0b01,
            Reserved(val) => val,
        }
    }
}

pub(crate) struct FeatVolatileWriteCache {
    pub wce: bool,
}
impl From<u32> for FeatVolatileWriteCache {
    fn from(cdw11: u32) -> Self {
        Self { wce: cdw11 & 0b1 != 0 }
    }
}
impl From<FeatVolatileWriteCache> for u32 {
    fn from(value: FeatVolatileWriteCache) -> Self {
        u32::from(value.wce)
    }
}

pub(crate) struct FeatNumberQueues {
    /// Number of I/O Completion Queues Requested/Allocated (NCQR/NCQA)
    ///
    /// Does not include Admin Completion Queue. Minimum of 2 shall be requested.
    pub ncq: u16,
    /// Number of I/O Submission Queues Requested/Allocated (NSQR/NSQA)
    ///
    /// Does not include Admin Submission Queue. Minimum of 2 shall be requested.
    pub nsq: u16,
}
impl TryFrom<u32> for FeatNumberQueues {
    type Error = ();

    fn try_from(cdw11: u32) -> Result<Self, Self::Error> {
        let ncqr = (cdw11 >> 16) as u16;
        let nsqr = cdw11 as u16;

        if ncqr == u16::MAX || nsqr == u16::MAX {
            // A max value of 65534 is allowed, implying 65535 queues of the
            // respective type(s).  Reject requests which exceed that.
            Err(())
        } else {
            Ok(Self {
                // Convert from 0's based values
                ncq: ncqr + 1,
                nsq: nsqr + 1,
            })
        }
    }
}
// Usable for both the return from SetFeature and GetFeature for NumberOfQueues,
// we reuse this struct as the format is the same.
impl From<FeatNumberQueues> for u32 {
    fn from(value: FeatNumberQueues) -> Self {
        // Convert to 0's based DW0
        (u32::from(value.ncq.saturating_sub(1)) << 16)
            | u32::from(value.nsq.saturating_sub(1))
    }
}

pub(crate) struct FeatInterruptVectorConfig {
    /// Interrupt Vector (IV)
    pub iv: u16,
    /// Coalescing Disable (CD)
    pub cd: bool,
}
impl From<u32> for FeatInterruptVectorConfig {
    fn from(cdw11: u32) -> Self {
        Self { iv: cdw11 as u16, cd: cdw11 & (1 << 16) != 0 }
    }
}
impl From<FeatInterruptVectorConfig> for u32 {
    fn from(value: FeatInterruptVectorConfig) -> Self {
        u32::from(value.iv) | (u32::from(value.cd) << 16)
    }
}

/// A parsed NVM Command
#[allow(dead_code)]
#[derive(Debug)]
pub enum NvmCmd {
    /// Commit data and metadata
    Flush,
    /// Write data and metadata
    Write(WriteCmd),
    /// Read data and metadata
    Read(ReadCmd),
    /// An unknown NVM command
    Unknown(GuestData<SubmissionQueueEntry>),
}

impl NvmCmd {
    /// Try to parse an `NvmCmd` out of a raw Submission Entry.
    pub fn parse(
        raw: GuestData<SubmissionQueueEntry>,
    ) -> Result<Self, ParseErr> {
        let _fuse = match (raw.cdw0 >> 8) & 0b11 {
            0b00 => Ok(()),               // Normal (non-fused) operation
            0b01 => Err(ParseErr::Fused), // First fused op
            0b10 => Err(ParseErr::Fused), // Second fused op
            _ => Err(ParseErr::ReservedFuse),
        }?;
        let cmd = match raw.opcode() {
            bits::NVM_OPC_FLUSH => NvmCmd::Flush,
            bits::NVM_OPC_WRITE => NvmCmd::Write(WriteCmd {
                slba: (u64::from(raw.cdw11) << 32) | u64::from(raw.cdw10),
                // Convert from 0's based value
                nlb: raw.cdw12 as u16 + 1,
                prp1: raw.prp1,
                prp2: raw.prp2,
            }),
            bits::NVM_OPC_READ => NvmCmd::Read(ReadCmd {
                slba: (u64::from(raw.cdw11) << 32) | u64::from(raw.cdw10),
                // Convert from 0's based value
                nlb: raw.cdw12 as u16 + 1,
                prp1: raw.prp1,
                prp2: raw.prp2,
            }),
            _ => NvmCmd::Unknown(raw),
        };
        Ok(cmd)
    }
}

/// Write Command Parameters
#[derive(Debug)]
pub struct WriteCmd {
    /// Starting LBA (SLBA)
    ///
    /// 64-bit base address of the first logical block to be written.
    pub slba: u64,

    /// Number of Logical Blocks (NLB)
    ///
    /// The number of logical blocks to be written.
    pub nlb: u16,

    /// PRP Entry 1 (PRP1)
    ///
    /// The first PRP entry specifying the start of the data buffer to be transferred from.
    prp1: u64,

    /// PRP Entry 2 (PRP2)
    ///
    /// If PRP1 specifies enough space, then PRP2 is reserved. Otherwise
    /// PRP2 may either be another PRP entry or a PRP list as necessary.
    prp2: u64,
}

impl WriteCmd {
    /// Returns an Iterator that yields [`GuestRegion`]'s to read the data to transfer out.
    pub fn data<'a>(&self, sz: u64, mem: &'a MemCtx) -> PrpIter<'a> {
        PrpIter::new(sz, self.prp1, self.prp2, mem)
    }
}

/// Read Command Parameters
#[derive(Debug)]
pub struct ReadCmd {
    /// Starting LBA (SLBA)
    ///
    /// 64-bit base address of the first logical block to be read.
    pub slba: u64,

    /// Number of Logical Blocks (NLB)
    ///
    /// The number of logical blocks to be read.
    pub nlb: u16,

    /// PRP Entry 1 (PRP1)
    ///
    /// The first PRP entry specifying the start of the data buffer to be transferred to.
    prp1: u64,

    /// PRP Entry 2 (PRP2)
    ///
    /// If PRP1 specifies enough space, then PRP2 is reserved. Otherwise
    /// PRP2 may either be another PRP entry or a PRP list as necessary.
    prp2: u64,
}

impl ReadCmd {
    /// Returns an Iterator that yields [`GuestRegion`]'s to write the data to transfer in.
    pub fn data<'a>(&self, sz: u64, mem: &'a MemCtx) -> PrpIter<'a> {
        PrpIter::new(sz, self.prp1, self.prp2, mem)
    }
}

/// Indicates the possible states of a [`PrpIter`].
#[derive(Clone, Copy, Eq, PartialEq, Debug)]
enum PrpNext {
    Prp1,
    Prp2,
    List(u64, u16),
    Done,
}

/// The last valid PRP list entry index in a single Physical Region Page (PRP) List
///
/// 512 64-bit entries in a PRP list
/// Note this relies on a Page Size of 4k (2^12).
/// More generally: 2^(lg(PAGE_SIZE)-1-2)
///     - 2 because PRP entries are expected to be 32-bit aligned.
///
/// See NVMe 1.0e Section 4.3 Physical Region Page Entry and List
const PRP_LIST_MAX: u16 = 511;

/// A helper object for iterator over a single, 2 or a list of PRPs.
pub struct PrpIter<'a> {
    /// PRP Entry 1 (PRP1)
    ///
    /// The first PRP entry specifying the start of the data buffer or
    prp1: u64,

    /// PRP Entry 2 (PRP2)
    ///
    /// The second PRP entry or a PRP list pointer or reserved if PRP1 was enough.
    prp2: u64,

    /// Handle to Guest's [`MemCtx`]
    mem: &'a MemCtx,

    /// How many bytes remaining to be read/written
    remain: u64,

    /// The next PRP state
    next: PrpNext,

    /// Any error we might've encountered
    error: Option<&'static str>,
}

impl<'a> PrpIter<'a> {
    /// Create a new `PrpIter` object.
    ///
    /// See corresponding `data` methods on any relevant commands.
    pub fn new(size: u64, prp1: u64, prp2: u64, mem: &'a MemCtx) -> Self {
        // prp1 and prp2 are expected to be 32-bit aligned
        assert!(prp1 & 0b11 == 0);
        assert!(prp2 & 0b11 == 0);
        Self { prp1, prp2, mem, remain: size, next: PrpNext::Prp1, error: None }
    }
}

impl PrpIter<'_> {
    /// Grab the next memory region to read/write
    fn get_next(&mut self) -> Result<GuestRegion, &'static str> {
        assert!(self.remain > 0);
        assert!(self.error.is_none());

        // PRP Entry Layout
        // | 63 . . . . . . . . . . . . . . . n + 1 | n . . . . . . 2 | 1 0 |
        // |       page base address                |      offset     | 0 0 |
        let (addr, size, next) = match self.next {
            PrpNext::Prp1 => {
                // The first PRP entry contained within the command may have a
                // non-zero offset within the memory page.
                probes::nvme_prp_entry!(|| (
                    self as *const Self as u64,
                    self.prp1
                ));
                let offset = self.prp1 & PAGE_OFFSET as u64;
                let size = u64::min(PAGE_SIZE as u64 - offset, self.remain);
                let after = self.remain - size;
                let next = if after == 0 {
                    PrpNext::Done
                } else if after <= PAGE_SIZE as u64 {
                    // Remaining data can be covered by single additional PRP
                    // entry which should be present in PRP2
                    PrpNext::Prp2
                } else {
                    // If the remaining length is larger than the page size,
                    // PRP2 points to a list.
                    //
                    // The first PRP List entry:
                    // - shall be Qword aligned, and
                    // - may also have a non-zero offset within the memory page.
                    if (self.prp2 % 8) != 0 {
                        return Err("PRP2 not Qword aligned!");
                    }

                    // PRP2 is allowed a non-zero offset into the page, meaning
                    // this operation's PRP list could start in the middle of
                    // the page. For example, idx below could be anywhere from 0
                    // to PRP_LIST_MAX:
                    //
                    //                | idx
                    //  --------------| ---
                    //  PRP List base | 0
                    //  PRP List base | 1
                    //  PRP List base | 2
                    //  ...
                    //  PRP List base | PRP_LIST_MAX - 1
                    //  PRP List base | PRP_LIST_MAX
                    //
                    // Note that lists cannot cross page boundaries. If idx =
                    // PRP_LIST_MAX is reached, the last entry will point to
                    // another list (unless the remaining size is satisfied by
                    // the end of the list).
                    let base = self.prp2 & (PAGE_MASK as u64);
                    let idx = (self.prp2 & PAGE_OFFSET as u64) / 8;
                    probes::nvme_prp_list!(|| (
                        self as *const Self as u64,
                        base,
                        idx as u16,
                    ));
                    PrpNext::List(base, idx as u16)
                };
                (self.prp1, size, next)
            }
            PrpNext::Prp2 => {
                // If a second PRP entry is present within a command, it shall
                // have a memory page offset of 0h
                if self.prp2 & PAGE_OFFSET as u64 != 0 {
                    return Err("Inappropriate PRP2 offset");
                }
                probes::nvme_prp_entry!(|| (
                    self as *const Self as u64,
                    self.prp2
                ));
                let size = self.remain;
                assert!(size <= PAGE_SIZE as u64);
                (self.prp2, size, PrpNext::Done)
            }
            PrpNext::List(base, idx) => {
                assert!(idx <= PRP_LIST_MAX);
                let entry_addr = base + u64::from(idx) * 8;
                let entry: GuestData<u64> = self
                    .mem
                    .read(GuestAddr(entry_addr))
                    .ok_or_else(|| "Unable to read PRP list entry")?;
                probes::nvme_prp_entry!(
                    || (self as *const Self as u64, entry,)
                );

                if *entry & PAGE_OFFSET as u64 != 0 {
                    return Err("Inappropriate PRP list entry offset");
                }

                if self.remain <= PAGE_SIZE as u64 {
                    (*entry, self.remain, PrpNext::Done)
                } else if idx != PRP_LIST_MAX {
                    (*entry, PAGE_SIZE as u64, PrpNext::List(base, idx + 1))
                } else {
                    // The last PRP in this list chains to another
                    // (page-aligned) list with the next PRP.
                    self.next = PrpNext::List(*entry, 0);
                    probes::nvme_prp_list!(|| (
                        self as *const Self as u64,
                        *entry,
                        0,
                    ));
                    return self.get_next();
                }
            }
            PrpNext::Done => {
                // prior checks of self.remain should prevent us from ever
                // reaching this
                panic!()
            }
        };

        assert!(size <= self.remain);
        if size == self.remain {
            assert_eq!(next, PrpNext::Done);
        }
        self.remain -= size;
        self.next = next;

        Ok(GuestRegion(GuestAddr(addr), size as usize))
    }
}

impl Iterator for PrpIter<'_> {
    type Item = GuestRegion;

    fn next(&mut self) -> Option<Self::Item> {
        if self.remain == 0 || self.error.is_some() {
            return None;
        }
        match self.get_next() {
            Ok(res) => Some(res),
            Err(e) => {
                probes::nvme_prp_error!(|| e);
                self.error = Some(e);
                None
            }
        }
    }
}

/// A Command Completion result
#[derive(Debug)]
pub struct Completion {
    /// Status Code Type and Status Code
    pub status: u16,
    /// Command Specific Result (DW0)
    pub dw0: u32,
}

impl Completion {
    /// Create a successful Completion result
    pub fn success() -> Self {
        Self {
            dw0: 0,
            status: Self::status_field(
                StatusCodeType::Generic,
                bits::STS_SUCCESS,
            ),
        }
    }

    /// Create a successful Completion result with a specific value
    pub fn success_val(cdw0: u32) -> Self {
        Self {
            dw0: cdw0,
            status: Self::status_field(
                StatusCodeType::Generic,
                bits::STS_SUCCESS,
            ),
        }
    }

    /// Create an error Completion result with a specific type and status
    pub fn specific_err(sct: StatusCodeType, status: u8) -> Self {
        // success doesn't belong in an error
        assert_ne!((sct, status), (StatusCodeType::Generic, bits::STS_SUCCESS));

        Self { dw0: 0, status: Self::status_field(sct, status) }
    }

    /// Create a generic error Completion result with a specific status
    pub fn generic_err(status: u8) -> Self {
        // success doesn't belong in an error
        assert_ne!(status, bits::STS_SUCCESS);

        Self {
            dw0: 0,
            status: Self::status_field(StatusCodeType::Generic, status),
        }
    }

    /// Set do-not-retry bit on a Completion already bearing an error status
    pub fn dnr(mut self) -> Self {
        assert_ne!(
            (self.status >> 1) as u8,
            bits::STS_SUCCESS,
            "cannot set DNR on non-error"
        );
        self.status |= 1 << 15;
        self
    }

    /// Helper method to combine [StatusCodeType] and status code
    const fn status_field(sct: StatusCodeType, sc: u8) -> u16 {
        ((sc as u16) << 1) | (((sct as u8) as u16) << 9)
        // ((more as u16) << 14) | ((dnr as u16) << 15)
    }
}

impl From<QueueCreateErr> for Completion {
    fn from(e: QueueCreateErr) -> Self {
        match e {
            QueueCreateErr::InvalidBaseAddr => {
                Completion::generic_err(bits::STS_INVAL_FIELD)
            }
            QueueCreateErr::InvalidSize => Completion::specific_err(
                StatusCodeType::CmdSpecific,
                bits::STS_CREATE_IO_Q_INVAL_QSIZE,
            ),
            QueueCreateErr::SubQueueIdAlreadyExists(_) => {
                Completion::specific_err(
                    StatusCodeType::CmdSpecific,
                    bits::STS_CREATE_IO_Q_INVAL_QID,
                )
            }
        }
    }
}

impl From<block::Result> for Completion {
    fn from(res: block::Result) -> Completion {
        match res {
            block::Result::Success => Completion::success(),
            block::Result::Failure => {
                Completion::generic_err(bits::STS_DATA_XFER_ERR)
            }
            block::Result::ReadOnly => Completion::specific_err(
                bits::StatusCodeType::CmdSpecific,
                bits::STS_WRITE_READ_ONLY_RANGE,
            ),
            block::Result::Unsupported => Completion::specific_err(
                bits::StatusCodeType::CmdSpecific,
                bits::STS_READ_CONFLICTING_ATTRS,
            ),
        }
    }
}

#[cfg(test)]
mod test {
    use crate::accessors::MemAccessor;
    use crate::common::*;
    use crate::vmm::mem::PhysMap;

    use super::PrpIter;

    const VM_SIZE: usize = 256 * PAGE_SIZE;
    const PRP_PER_PAGE: usize = PAGE_SIZE / 8;

    fn setup() -> (PhysMap, MemAccessor) {
        let mut pmap = PhysMap::new_test(VM_SIZE);
        pmap.add_test_mem("lowmem".to_string(), 0, VM_SIZE)
            .expect("lowmem seg creation should succeed");

        let acc_mem = pmap.finalize();
        (pmap, acc_mem)
    }

    // Simple helpers to make math below more terse
    const fn region(addr: u64, sz: u64) -> GuestRegion {
        GuestRegion(GuestAddr(addr), sz as usize)
    }
    const fn pages(c: u64) -> u64 {
        PAGE_SIZE as u64 * c
    }

    #[test]
    fn test_prp_single() {
        let (_pmap, acc_mem) = setup();
        let memctx = acc_mem.access().unwrap();

        // Basic single page
        let mut iter = PrpIter::new(pages(1), 0x1000, 0, &memctx);
        assert_eq!(iter.next(), Some(region(0x1000, pages(1))));
        assert_eq!(iter.next(), None);

        // Sub-single page
        let sub = 0x200;
        let mut iter = PrpIter::new(pages(1) - sub, 0x1000, 0, &memctx);
        assert_eq!(iter.next(), Some(region(0x1000, pages(1) - sub)));
        assert_eq!(iter.next(), None);

        // Sub-single page (with offset)
        let mut iter = PrpIter::new(pages(1) - sub, 0x1000 + sub, 0, &memctx);
        assert_eq!(iter.next(), Some(region(0x1000 + sub, pages(1) - sub)));
        assert_eq!(iter.next(), None);
    }

    #[test]
    fn test_prp_dual() {
        let (_pmap, acc_mem) = setup();
        let memctx = acc_mem.access().unwrap();

        // Basic dual page
        let mut iter = PrpIter::new(pages(2), 0x1000, 0x2000, &memctx);
        assert_eq!(iter.next(), Some(region(0x1000, pages(1))));
        assert_eq!(iter.next(), Some(region(0x2000, pages(1))));
        assert_eq!(iter.next(), None);

        // single page, split by offset
        let sub = 0x200;
        let mut iter = PrpIter::new(pages(1), 0x1000 + sub, 0x2000, &memctx);
        assert_eq!(iter.next(), Some(region(0x1000 + sub, pages(1) - sub)));
        assert_eq!(iter.next(), Some(region(0x2000, sub)));
        assert_eq!(iter.next(), None);

        // less than single page, split by offset
        let sz = pages(1) - 0x200;
        let off = 0x400;
        let mut iter = PrpIter::new(sz, 0x1000 + off, 0x2000, &memctx);
        assert_eq!(iter.next(), Some(region(0x1000 + off, pages(1) - off)));
        assert_eq!(iter.next(), Some(region(0x2000, sz - (pages(1) - off))));
        assert_eq!(iter.next(), None);
    }

    #[test]
    fn test_prp_list() {
        // Basic triple page (aligned prplist)
        let (_pmap, acc_mem) = setup();
        let memctx = acc_mem.access().unwrap();

        let listprps: [u64; 2] = [0x2000, 0x3000];
        let listaddr = 0x80000;
        memctx.write(GuestAddr(listaddr), &listprps);
        let mut iter = PrpIter::new(pages(3), 0x1000, listaddr, &memctx);
        assert_eq!(iter.next(), Some(region(0x1000, pages(1))));
        assert_eq!(iter.next(), Some(region(0x2000, pages(1))));
        assert_eq!(iter.next(), Some(region(0x3000, pages(1))));
        assert_eq!(iter.next(), None);

        // Basic triple page (offset prplist)
        let (_pmap, acc_mem) = setup();
        let memctx = acc_mem.access().unwrap();

        let listprps: [u64; 2] = [0x2000, 0x3000];
        let listaddr = 0x80010;
        memctx.write(GuestAddr(listaddr), &listprps);
        let mut iter = PrpIter::new(pages(3), 0x1000, listaddr, &memctx);
        assert_eq!(iter.next(), Some(region(0x1000, pages(1))));
        assert_eq!(iter.next(), Some(region(0x2000, pages(1))));
        assert_eq!(iter.next(), Some(region(0x3000, pages(1))));
        assert_eq!(iter.next(), None);

        // Offset triple page
        let (_pmap, acc_mem) = setup();
        let memctx = acc_mem.access().unwrap();

        let listprps: [u64; 3] = [0x2000, 0x3000, 0x4000];
        let listaddr = 0x80000;
        let off = 0x200;
        memctx.write(GuestAddr(listaddr), &listprps);
        let mut iter = PrpIter::new(pages(3), 0x1000 + off, listaddr, &memctx);
        assert_eq!(iter.next(), Some(region(0x1000 + off, pages(1) - off)));
        assert_eq!(iter.next(), Some(region(0x2000, pages(1))));
        assert_eq!(iter.next(), Some(region(0x3000, pages(1))));
        assert_eq!(iter.next(), Some(region(0x4000, off)));
        assert_eq!(iter.next(), None);
    }

    #[test]
    fn test_prp_list_offset_last() {
        // List with offset, where last entry covers less than one page
        let (_pmap, acc_mem) = setup();
        let memctx = acc_mem.access().unwrap();

        let listaddr = 0x80000u64;
        let mut prps: Vec<u64> = Vec::with_capacity(PRP_PER_PAGE);
        let mut bufaddr = 0x2000u64;
        for _idx in 0..PRP_PER_PAGE {
            prps.push(bufaddr);
            bufaddr += pages(1);
        }
        memctx.write_many(GuestAddr(listaddr), &prps);
        let off = 0x200;
        let mut iter = PrpIter::new(
            pages(PRP_PER_PAGE as u64),
            0x1000 + off,
            listaddr,
            &memctx,
        );
        assert_eq!(
            iter.next(),
            Some(region(0x1000 + off, pages(1) - off)),
            "prp1 entry incorrect"
        );

        let mut bufaddr = 0x2000u64;
        for idx in 0..(PRP_PER_PAGE - 1) {
            assert_eq!(
                iter.next(),
                Some(region(bufaddr, pages(1))),
                "bad prp at idx: {idx}"
            );
            bufaddr += pages(1);
        }
        // last prplist entry should be the remaining bytes left over from
        // offsetting the prp1 entry
        assert_eq!(iter.next(), Some(region(bufaddr, off)));
    }

    #[test]
    fn test_prp_multiple() {
        // Basic multiple-page prplist
        let (_pmap, acc_mem) = setup();
        let memctx = acc_mem.access().unwrap();

        let listaddrs = [0x80000u64, 0x81000u64];
        let mut prps: Vec<u64> = Vec::with_capacity(PRP_PER_PAGE);
        let mut bufaddr = 0x2000u64;

        let entries_first = PRP_PER_PAGE - 1;
        for _idx in 0..entries_first {
            prps.push(bufaddr);
            bufaddr += pages(1);
        }
        // Link to the next list page
        prps.push(listaddrs[1]);
        memctx.write_many(GuestAddr(listaddrs[0]), &prps[..]);

        // populate a few more entries in the next prplist
        let entries_second = 4;
        for idx in 0..entries_second {
            prps[idx] = bufaddr;
            bufaddr += pages(1);
        }
        memctx.write_many(GuestAddr(listaddrs[1]), &prps[..entries_second]);

        let total_entries = 1 + entries_first + entries_second;
        let mut iter = PrpIter::new(
            pages(total_entries as u64),
            0x1000,
            listaddrs[0],
            &memctx,
        );

        let mut bufaddr = 0x1000u64;
        for idx in 0..total_entries {
            assert_eq!(
                iter.next(),
                Some(region(bufaddr, pages(1))),
                "bad prp at idx: {idx}"
            );
            bufaddr += pages(1);
        }
        assert_eq!(iter.next(), None);
    }
}
