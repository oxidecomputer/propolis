use super::bits::{self, RawSubmission, StatusCodeType};
use super::queue::{QueueCreateErr, QueueId};
use crate::common::*;
use crate::hw::nvme::bits::STS_INVAL_PRP_OFFSET;
use crate::vmm::MemCtx;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum ParseErr {
    #[error("SGLs not supported")]
    SGL,

    #[error("Fused ops not supported")]
    Fused,

    #[error("reserved PSDT value specified")]
    ReservedPsdt,

    #[error("reserved FUSE value specified")]
    ReservedFuse,
}

#[derive(Debug)]
pub enum AdminCmd {
    DeleteIOSubQ(QueueId),
    CreateIOSubQ(CreateIOSQCmd),
    GetLogPage(GetLogPageCmd),
    DeleteIOCompQ(QueueId),
    CreateIOCompQ(CreateIOCQCmd),
    Identify(IdentifyCmd),
    Abort,
    SetFeatures(SetFeaturesCmd),
    GetFeatures,
    AsyncEventReq,
    Unknown(RawSubmission),
}
impl AdminCmd {
    pub fn parse(
        raw: RawSubmission,
    ) -> Result<(Self, SubmissionEntry), ParseErr> {
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
                    qsize: (raw.cdw10 >> 16) as u16,
                    qid: raw.cdw10 as u16,
                    cqid: (raw.cdw11 >> 16) as u16,
                    queue_prio,
                    phys_contig: (raw.cdw11 & 1) != 0,
                })
            }
            bits::ADMIN_OPC_GET_LOG_PAGE => {
                AdminCmd::GetLogPage(GetLogPageCmd {
                    nsid: raw.nsid,
                    len: (((raw.cdw11 as u16) as u32) << 16
                        | (raw.cdw10 >> 16))
                        * 4,
                    retain_async_ev: (raw.cdw10 & (1 << 15) != 0),
                    log_specific_field: (raw.cdw10 >> 8) as u8 & 0b1111,
                    log_page_ident: LogPageIdent::from(raw.cdw10 as u8),
                    log_page_offset: raw.cdw12 as u64
                        | (raw.cdw13 as u64) << 32,
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
                    qsize: (raw.cdw10 >> 16) as u16,
                    qid: raw.cdw10 as u16,
                    intr_vector: (raw.cdw11 >> 16) as u16,
                    intr_enable: (raw.cdw11 & 0b10) != 0,
                    phys_contig: (raw.cdw11 & 0b1) != 0,
                })
            }
            bits::ADMIN_OPC_IDENTIFY => AdminCmd::Identify(IdentifyCmd {
                cns: raw.cdw10 as u8,
                cntid: (raw.cdw10 >> 16) as u16,
                nsid: raw.nsid,
                prp1: raw.prp1,
                prp2: raw.prp2,
            }),
            bits::ADMIN_OPC_ABORT => AdminCmd::Abort,
            bits::ADMIN_OPC_SET_FEATURES => {
                AdminCmd::SetFeatures(SetFeaturesCmd {
                    save: (raw.cdw10 & (1 << 31)) != 0,
                    fid: FeatureIdent::from((raw.cdw10 as u8, raw.cdw11)),
                })
            }
            bits::ADMIN_OPC_GET_FEATURES => AdminCmd::GetFeatures,
            bits::ADMIN_OPC_ASYNC_EVENT_REQ => AdminCmd::AsyncEventReq,
            _ => AdminCmd::Unknown(raw),
        };
        let _psdt = match (raw.cdw0 >> 14) & 0b11 {
            0b00 => Ok(()),             // PRP
            0b01 => Err(ParseErr::SGL), // SGL buffer
            0b10 => Err(ParseErr::SGL), // SGL segment
            _ => Err(ParseErr::ReservedPsdt),
        }?;
        let _fuse = match (raw.cdw0 >> 8) & 0b11 {
            0b00 => Ok(()),               // Normal (non-fused) operation
            0b01 => Err(ParseErr::Fused), // First fused op
            0b10 => Err(ParseErr::Fused), // Second fused op
            _ => Err(ParseErr::ReservedFuse),
        }?;
        Ok((cmd, SubmissionEntry::new(&raw)))
    }
}

pub struct SubmissionEntry {
    pub cid: u16,
    pub prp1: u64,
    pub prp2: u64,
}
impl SubmissionEntry {
    fn new(raw: &RawSubmission) -> Self {
        Self { cid: raw.cid(), prp1: raw.prp1, prp2: raw.prp2 }
    }
}

#[derive(Debug)]
pub struct CreateIOCQCmd {
    pub prp: u64,
    pub qsize: u16,
    pub qid: QueueId,
    pub intr_vector: u16,
    pub intr_enable: bool,
    pub phys_contig: bool,
}
#[derive(Debug)]
pub struct CreateIOSQCmd {
    pub prp: u64,
    pub qsize: u16,
    pub qid: QueueId,
    pub cqid: QueueId,
    pub queue_prio: QueuePriority,
    pub phys_contig: bool,
}

#[derive(Debug)]
pub enum QueuePriority {
    Urgent,
    High,
    Medium,
    Low,
}

#[derive(Debug)]
pub struct GetLogPageCmd {
    pub nsid: u32,
    pub len: u32,
    pub retain_async_ev: bool,
    pub log_specific_field: u8,
    pub log_page_ident: LogPageIdent,
    pub log_page_offset: u64,
    prp1: u64,
    prp2: u64,
}

impl GetLogPageCmd {
    pub fn data<'a>(&'a self, mem: MemCtx<'a>) -> PrpIter<'a> {
        PrpIter::new(PAGE_SIZE as u64, self.prp1, self.prp2, mem)
    }
}

#[derive(Debug)]
pub enum LogPageIdent {
    Reserved,
    Error,
    Smart,
    Firmware,
    Reservation,
    SanitizeStatus,
    Vendor(u8),
}

impl From<u8> for LogPageIdent {
    fn from(ident: u8) -> Self {
        match ident {
            0 => LogPageIdent::Reserved,
            1 => LogPageIdent::Error,
            2 => LogPageIdent::Smart,
            3 => LogPageIdent::Firmware,
            4..=8 => LogPageIdent::Reserved,
            9..=0x6F => LogPageIdent::Reserved,
            0x70 => LogPageIdent::Reserved,
            0x71..=0x7F => LogPageIdent::Reserved,
            0x80 => LogPageIdent::Reservation,
            0x81 => LogPageIdent::SanitizeStatus,
            0x82..=0xBF => LogPageIdent::Reserved,
            0xC0..=0xFF => LogPageIdent::Vendor(ident),
        }
    }
}

#[derive(Debug)]
pub struct IdentifyCmd {
    pub cns: u8,
    pub cntid: u16,
    pub nsid: u32,
    prp1: u64,
    prp2: u64,
}
impl IdentifyCmd {
    pub fn data<'a>(&'a self, mem: MemCtx<'a>) -> PrpIter<'a> {
        PrpIter::new(PAGE_SIZE as u64, self.prp1, self.prp2, mem)
    }
}

#[derive(Debug)]
pub struct SetFeaturesCmd {
    pub save: bool,
    pub fid: FeatureIdent,
}

#[derive(Debug)]
pub enum FeatureIdent {
    Reserved,
    Arbitration,
    PowerManagement,
    LbaRangeType,
    TemperatureThreshold,
    ErrorRecovery,
    VolatileWriteCache,
    NumberOfQueues {
        /// Number of I/O Completion Queues Requested
        ncqr: u16,
        /// Number of I/O Submission Queues Requested
        nsqr: u16,
    },
    InterruptCoalescing,
    InterruptVectorConfiguration,
    WriteAtomicityNormal,
    AsynchronousEventConfiguration,
    AutonomousPowerStateTransition,
    HostMemoryBuffer,
    Timestamp,
    KeepAliveTimer,
    HostControlledThermalManagement,
    NonOperationPowerStateConfig,
    Managment(u8),
    SoftwareProgressMarker,
    HostIdentifier,
    ReservationNotificationMask,
    ReservationPersistance,
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
                ncqr: (cdw11 >> 16) as u16,
                nsqr: cdw11 as u16,
            },
            8 => InterruptCoalescing,
            9 => InterruptVectorConfiguration,
            0xA => WriteAtomicityNormal,
            0xB => AsynchronousEventConfiguration,
            0xC => AutonomousPowerStateTransition,
            0xD => HostMemoryBuffer,
            0xE => Timestamp,
            0xF => KeepAliveTimer,
            0x10 => HostControlledThermalManagement,
            0x11 => NonOperationPowerStateConfig,
            0x12..=0x77 => Reserved,
            0x78..=0x7F => Managment(id),
            0x80 => SoftwareProgressMarker,
            0x81 => HostIdentifier,
            0x82 => ReservationNotificationMask,
            0x83 => ReservationPersistance,
            0x84..=0xBF => Reserved,
            0xC0..=0xFF => Vendor(id),
        }
    }
}

#[derive(Debug)]
pub enum NvmCmd {
    Flush,
    Write(WriteCmd),
    Read(ReadCmd),
    Unknown(RawSubmission),
}

impl NvmCmd {
    pub fn parse(
        raw: RawSubmission,
    ) -> Result<(Self, SubmissionEntry), ParseErr> {
        let _psdt = match (raw.cdw0 >> 14) & 0b11 {
            0b00 => Ok(()),             // PRP
            0b01 => Err(ParseErr::SGL), // SGL buffer
            0b10 => Err(ParseErr::SGL), // SGL segment
            _ => Err(ParseErr::ReservedPsdt),
        }?;
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
                nlb: raw.cdw12 as u16,
                prp1: raw.prp1,
                prp2: raw.prp2,
            }),
            bits::NVM_OPC_READ => NvmCmd::Read(ReadCmd {
                slba: (raw.cdw11 as u64) << 32 | raw.cdw10 as u64,
                nlb: raw.cdw12 as u16,
                prp1: raw.prp1,
                prp2: raw.prp2,
            }),
            _ => NvmCmd::Unknown(raw),
        };
        Ok((cmd, SubmissionEntry::new(&raw)))
    }
}

#[derive(Debug)]
pub struct WriteCmd {
    pub slba: u64,
    pub nlb: u16,
    prp1: u64,
    prp2: u64,
}

impl WriteCmd {
    pub fn data<'a>(&'a self, sz: u64, mem: MemCtx<'a>) -> PrpIter<'a> {
        PrpIter::new(sz, self.prp1, self.prp2, mem)
    }
}

#[derive(Debug)]
pub struct ReadCmd {
    pub slba: u64,
    pub nlb: u16,
    prp1: u64,
    prp2: u64,
}

impl ReadCmd {
    pub fn data<'a>(&'a self, sz: u64, mem: MemCtx<'a>) -> PrpIter<'a> {
        PrpIter::new(sz, self.prp1, self.prp2, mem)
    }
}

#[derive(Clone, Copy, Eq, PartialEq, Debug)]
enum PrpNext {
    Prp1,
    Prp2,
    List(u64, u16),
    Done,
}

// 512 64-bit entries in a PRP list
const PRP_LIST_MAX: u16 = 511;

pub struct PrpIter<'a> {
    prp1: u64,
    prp2: u64,
    mem: MemCtx<'a>,
    remain: u64,
    next: PrpNext,
    error: Option<&'static str>,
}
impl<'a> PrpIter<'a> {
    pub fn new(size: u64, prp1: u64, prp2: u64, mem: MemCtx<'a>) -> Self {
        // prp1 and prp2 are expected to be 32-bit aligned
        assert!(prp1 & 0b11 == 0);
        assert!(prp2 & 0b11 == 0);
        Self { prp1, prp2, mem, remain: size, next: PrpNext::Prp1, error: None }
    }
}

impl PrpIter<'_> {
    fn get_next(&mut self) -> Result<GuestRegion, &'static str> {
        assert!(self.remain > 0);
        assert!(self.error.is_none());

        let (addr, size, next) = match self.next {
            PrpNext::Prp1 => {
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
                    let list_off = (self.prp2 & PAGE_OFFSET as u64) / 8;
                    PrpNext::List(self.prp2, list_off as u16)
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

#[derive(Debug)]
pub struct Completion {
    /// Status Code Type and Status Code
    pub status: u16,
    pub cdw0: u32,
}

impl Completion {
    pub fn success() -> Self {
        Self {
            cdw0: 0,
            status: Self::status_field(
                StatusCodeType::Generic,
                bits::STS_SUCCESS,
            ),
        }
    }
    pub fn success_val(cdw0: u32) -> Self {
        Self {
            cdw0,
            status: Self::status_field(
                StatusCodeType::Generic,
                bits::STS_SUCCESS,
            ),
        }
    }

    pub fn specific_err(sct: StatusCodeType, status: u8) -> Self {
        // success doesn't belong in an error
        assert_ne!(status, bits::STS_SUCCESS);

        Self { cdw0: 0, status: Self::status_field(sct, status) }
    }

    pub fn generic_err(status: u8) -> Self {
        // success doesn't belong in an error
        assert_ne!(status, bits::STS_SUCCESS);

        Self {
            cdw0: 0,
            status: Self::status_field(StatusCodeType::Generic, status),
        }
    }

    fn status_field(sct: StatusCodeType, sc: u8) -> u16 {
        (sc as u16) << 1 | ((sct as u8) as u16) << 9
        // | (more as u16) << 14
        // | (dnr as u16) << 15
    }
}

impl From<QueueCreateErr> for Completion {
    fn from(e: QueueCreateErr) -> Self {
        match e {
            QueueCreateErr::InvalidBaseAddr => {
                Completion::generic_err(STS_INVAL_PRP_OFFSET)
            }
            QueueCreateErr::InvalidSize => Completion::specific_err(
                StatusCodeType::CmdSpecific,
                bits::STS_CREATE_IO_Q_INVAL_QSIZE,
            ),
        }
    }
}
