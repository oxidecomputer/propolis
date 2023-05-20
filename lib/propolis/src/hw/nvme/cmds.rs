use super::bits::{self, RawSubmission, StatusCodeType};
use super::queue::{QueueCreateErr, QueueId};
use crate::block;
use crate::common::*;
use crate::vmm::MemCtx;

use thiserror::Error;

/// Errors that may be encounted during command parsing.
#[derive(Debug, Error)]
pub enum ParseErr {
    /// Encounted a fused operation which we don't currently support.
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
    Abort,
    /// Set Features Command
    SetFeatures(SetFeaturesCmd),
    /// Get Features Command
    GetFeatures,
    /// Asynchronous Event Request Command
    AsyncEventReq,
    /// An unknown admin command
    Unknown(RawSubmission),
}

impl AdminCmd {
    /// Triy to parse an `AdminCmd` out of a raw Submission Entry.
    pub fn parse(raw: RawSubmission) -> Result<Self, ParseErr> {
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
            bits::ADMIN_OPC_ABORT => AdminCmd::Abort,
            bits::ADMIN_OPC_SET_FEATURES => {
                AdminCmd::SetFeatures(SetFeaturesCmd {
                    fid: FeatureIdent::from((raw.cdw10 as u8, raw.cdw11)),
                })
            }
            bits::ADMIN_OPC_GET_FEATURES => AdminCmd::GetFeatures,
            bits::ADMIN_OPC_ASYNC_EVENT_REQ => AdminCmd::AsyncEventReq,
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
    /// Returns an Iterator that yields [`GuestRegion`]'s to write the log page data to.
    pub fn data<'a>(&self, mem: &'a MemCtx) -> PrpIter<'a> {
        PrpIter::new(PAGE_SIZE as u64, self.prp1, self.prp2, mem)
    }
}

/// The type of Log pages that may be retrieved with the Get Log Page command.
///
/// See NVMe 1.0e Section 5.10.1, Figure 58 Get Log Page - Log Page Identifiers
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

/// Set Features Command Parameters
#[derive(Debug)]
pub struct SetFeaturesCmd {
    /// Feature Identifier (FID)
    ///
    /// The feature that attributes are being specified for.
    pub fid: FeatureIdent,
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
    /// Indicates the type and attribtues of LBA ranges that part of the specified namespace.
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
    /// Only allowed during initialization and canot change between resets.
    NumberOfQueues {
        /// Number of I/O Completion Queues Requested (NCQR)
        ///
        /// Does not include Admin Completion Queue. Minimum of 2 shall be requested.
        ncqr: u16,
        /// Number of I/O Submission Queues Requested
        ///
        /// Does not include Admin Submission Queue. Minimum of 2 shall be requested.
        nsqr: u16,
    },
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
    /// This feature is persistnt across power states.
    /// See NVMe 1.0e Section 7.6.1.1 Software Progress Marker
    SoftwareProgressMarker,
    /// Vendor specific feature.
    Vendor(u8),
}

impl From<(u8, u32)> for FeatureIdent {
    fn from((id, cdw11): (u8, u32)) -> Self {
        use FeatureIdent::*;
        match id {
            0 => Reserved,
            1 => Arbitration,
            2 => PowerManagement,
            3 => LbaRangeType,
            4 => TemperatureThreshold,
            5 => ErrorRecovery,
            6 => VolatileWriteCache,
            7 => NumberOfQueues {
                // Convert from 0's based values
                ncqr: (cdw11 >> 16) as u16 + 1,
                nsqr: cdw11 as u16 + 1,
            },
            8 => InterruptCoalescing,
            9 => InterruptVectorConfiguration,
            0xA => WriteAtomicity,
            0xB => AsynchronousEventConfiguration,
            0xC..=0x7F => Reserved,
            0x80 => SoftwareProgressMarker,
            0x81..=0xBF => Reserved,
            0xC0..=0xFF => Vendor(id),
        }
    }
}

/// A parsed NVM Command
#[derive(Debug)]
pub enum NvmCmd {
    /// Commit data and metadata
    Flush,
    /// Write data and metadata
    Write(WriteCmd),
    /// Read data and metadata
    Read(ReadCmd),
    /// An unknown NVM command
    Unknown(RawSubmission),
}

impl NvmCmd {
    /// Triy to parse an `NvmCmd` out of a raw Submission Entry.
    pub fn parse(raw: RawSubmission) -> Result<Self, ParseErr> {
        let _fuse = match (raw.cdw0 >> 8) & 0b11 {
            0b00 => Ok(()),               // Normal (non-fused) operation
            0b01 => Err(ParseErr::Fused), // First fused op
            0b10 => Err(ParseErr::Fused), // Second fused op
            _ => Err(ParseErr::ReservedFuse),
        }?;
        let cmd = match raw.opcode() {
            bits::NVM_OPC_FLUSH => NvmCmd::Flush,
            bits::NVM_OPC_WRITE => NvmCmd::Write(WriteCmd {
                slba: (raw.cdw11 as u64) << 32 | raw.cdw10 as u64,
                // Convert from 0's based value
                nlb: raw.cdw12 as u16 + 1,
                prp1: raw.prp1,
                prp2: raw.prp2,
            }),
            bits::NVM_OPC_READ => NvmCmd::Read(ReadCmd {
                slba: (raw.cdw11 as u64) << 32 | raw.cdw10 as u64,
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
        // | 63 . . . . . . . . . . . . . . . . . . . n + 1 | n . . . . . . 2 | 1 0 |
        // |         page base address                      |      offset     | 0 0 |
        let (addr, size, next) = match self.next {
            PrpNext::Prp1 => {
                // The first PRP entry contained within the command may have a non-zero offset
                // within the memory page.
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
                    // If the remaining length is larger than the page size, PRP2 points to a list.
                    //
                    // The first PRP List entry:
                    // - shall be Qword aligned, and
                    // - may also have a non-zero offset within the memory page.
                    if (self.prp2 % 8) != 0 {
                        return Err("PRP2 not Qword aligned!");
                    }

                    // PRP2 is allowed a non-zero offset into the page, meaning this operation's
                    // PRP list could start in the middle of the page. For example, idx below could
                    // be anywhere from 0 to PRP_LIST_MAX:
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
                    // Note that lists cannot cross page boundaries. If idx = PRP_LIST_MAX is
                    // reached, the last entry will point to another list (unless the remaining
                    // size is satisfied by the end of the list).
                    let base = self.prp2 & (PAGE_MASK as u64);
                    let idx = (self.prp2 & PAGE_OFFSET as u64) / 8;
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
                let size = self.remain;
                assert!(size <= PAGE_SIZE as u64);
                (self.prp2, size, PrpNext::Done)
            }
            PrpNext::List(base, idx) => {
                assert!(idx <= PRP_LIST_MAX);
                let entry_addr = base + (idx as u64) * 8;
                let entry: u64 = self
                    .mem
                    .read(GuestAddr(entry_addr))
                    .ok_or_else(|| "Unable to read PRP list entry")?;

                if entry & PAGE_OFFSET as u64 != 0 {
                    return Err("Inappropriate PRP list entry offset");
                }

                if self.remain <= PAGE_SIZE as u64 {
                    (entry, self.remain, PrpNext::Done)
                } else {
                    if idx != PRP_LIST_MAX {
                        (entry, PAGE_SIZE as u64, PrpNext::List(base, idx + 1))
                    } else {
                        // Chase the PRP to the next PRP list and read the first entry from it to
                        // use as the next result.
                        let next_entry: u64 =
                            self.mem.read(GuestAddr(entry)).ok_or_else(
                                || "Unable to read PRP list entry",
                            )?;
                        if next_entry & PAGE_OFFSET as u64 != 0 {
                            return Err("Inappropriate PRP list entry offset");
                        }
                        (next_entry, PAGE_SIZE as u64, PrpNext::List(entry, 1))
                    }
                }
            }
            PrpNext::Done => {
                // prior checks of self.remain should prevent us from ever reaching this
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
                0,
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
                0,
            ),
        }
    }

    /// Create an error Completion result with a specific type and status
    pub fn specific_err(sct: StatusCodeType, status: u8) -> Self {
        // success doesn't belong in an error
        assert_ne!(status, bits::STS_SUCCESS);

        Self { dw0: 0, status: Self::status_field(sct, status, 0) }
    }

    /// Create a generic error Completion result with a specific status
    pub fn generic_err(status: u8) -> Self {
        // success doesn't belong in an error
        assert_ne!(status, bits::STS_SUCCESS);

        Self {
            dw0: 0,
            status: Self::status_field(StatusCodeType::Generic, status, 0),
        }
    }

    /// Create a generic error Completion result with a specific status
    /// and the do-not-retry bit set.
    pub fn generic_err_dnr(status: u8) -> Self {
        // success doesn't belong in an error
        assert_ne!(status, bits::STS_SUCCESS);

        Self {
            dw0: 0,
            status: Self::status_field(StatusCodeType::Generic, status, 1),
        }
    }

    /// Helper method to combine StatusCodeType and status code
    fn status_field(sct: StatusCodeType, sc: u8, dnr: u8) -> u16 {
        (sc as u16) << 1 | ((sct as u8) as u16) << 9 | (dnr as u16) << 15
        // | (more as u16) << 14
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
            block::Result::Unsupported => Completion::specific_err(
                bits::StatusCodeType::CmdSpecific,
                bits::STS_READ_CONFLICTING_ATTRS,
            ),
        }
    }
}
