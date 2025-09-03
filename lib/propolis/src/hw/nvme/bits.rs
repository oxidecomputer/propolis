// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

#![allow(dead_code)]

use bitstruct::bitstruct;
use zerocopy::FromBytes;

/// A Submission Queue Entry as represented in memory.
///
/// See NVMe 1.0e Section 4.2 Submission Queue Entry - Command Format
#[derive(Debug, Default, Copy, Clone, FromBytes)]
#[repr(C, packed(1))]
pub struct SubmissionQueueEntry {
    /// Command Dword 0 (CDW0)
    ///
    /// Field common to all commands and defined as:
    ///
    /// Bits
    /// 31:16 - Command Identifier (CID)
    /// 15:10 - Reserved
    /// 09:08 - Fused Operation (FUSE)
    /// 07:00 - Opcode (OPC)
    ///
    /// See NVMe 1.0e Section 4.2, Figure 6 Command Dword 0
    pub cdw0: u32,

    /// Namespace Identifier (NSID)
    ///
    /// The namespace that this command applies to.
    pub nsid: u32,

    /// Reserved - Bytes 15:08
    pub rsvd: u64,

    /// Metadata Pointer (MPTR)
    ///
    /// If the command has metadata not interleaved with the logical
    /// block data, then this field contains the address of a contiguous
    /// physical buffer of metadata. The metadata pointer shall be
    /// DWORD aligned.
    ///
    /// See NVMe 1.0e Section 4.4 Metadata Region (MR)
    pub mptr: u64,

    /// The first Physical Region Page (PRP) entry for the command or a
    /// PRP List pointer.
    ///
    /// See NVMe 1.0e Section 4.3 Physical Region Page Entry and List
    pub prp1: u64,

    /// Either reserved, the second Physical Region Page (PRP) entry or
    /// a PRP List pointer.
    ///
    /// See NVMe 1.0e Section 4.3 Physical Region Page Entry and List
    pub prp2: u64,

    /// Command Dword 10 (CDW10)
    ///
    /// A command specific value.
    pub cdw10: u32,

    /// Command Dword 11 (CDW11)
    ///
    /// A command specific value.
    pub cdw11: u32,

    /// Command Dword 12 (CDW12)
    ///
    /// A command specific value.
    pub cdw12: u32,

    /// Command Dword 13 (CDW13)
    ///
    /// A command specific value.
    pub cdw13: u32,

    /// Command Dword 14 (CDW14)
    ///
    /// A command specific value.
    pub cdw14: u32,

    /// Command Dword 15 (CDW15)
    ///
    /// A command specific value.
    pub cdw15: u32,
}

impl SubmissionQueueEntry {
    /// Returns the Identifier (CID) of this Submission Queue Entry.
    ///
    /// The command identifier along with the Submission Queue ID
    /// specifiy a unique identifier for the command.
    pub fn cid(&self) -> u16 {
        (self.cdw0 >> 16) as u16
    }

    /// Returns the Opcode (OPC) of this Submission Queue Entry.
    pub fn opcode(&self) -> u8 {
        self.cdw0 as u8
    }
}

/// A Completion Queue Entry as represented in memory.
///
/// See NVMe 1.0e Section 4.5 Completion Queue Entry
#[derive(Debug, Default, Copy, Clone)]
#[repr(C, packed(1))]
pub struct CompletionQueueEntry {
    /// Dword 0 (DW0)
    ///
    /// A command specific value.
    pub dw0: u32,

    /// Reserved (DW1) - Bytes 07:04
    pub rsvd: u32,

    /// Submission Queue Head Pointer (SQHD)
    ///
    /// Indicates the current Submission Queue Head pointer
    /// for the Submission Queue identified by `sqid`.
    ///
    /// Bits 15:0 of Dword 2 (DW2)
    ///
    /// See NVMe 1.0e Section 4.5, Figure 13 Completion Queue Entry: DW 2
    pub sqhd: u16,

    /// Submission Queue Identifier (SQID)
    ///
    /// Indicates the Submission Queue for which the command completed
    /// by this Completion Entry was submitted to.
    ///
    /// Bits 31:16 of Dword 2 (DW2)
    ///
    /// See NVMe 1.0e Section 4.5, Figure 13 Completion Queue Entry: DW 2
    pub sqid: u16,

    /// Command Identifier (CID)
    ///
    /// The identifier of the command completed by this Completion Entry.
    /// The command identifier along with the Submission Queue ID
    /// specifiy a unique identifier for the command.
    ///
    /// Bits 15:0 of Dword 3 (DW3)
    ///
    /// See NVMe 1.0e Section 4.5, Figure 14 Completion Queue Entry: DW 3
    pub cid: u16,

    /// The status of the command that's being completed along with
    /// the current phase tag.
    ///
    /// Bit      0 Phase Tag (P)      ===  Bit 16 of Dword 3 (DW3)
    /// Bits 15:01 Status Field (SF)  ===  Bits 31:17 of Dword 3 (DW3)
    ///
    /// See NVMe 1.0e Section 4.5.1 Status Field Definition
    /// See NVMe 1.0e Section 4.5, Figure 14 Completion Queue Entry: DW 3
    pub status_phase: u16,
}
impl CompletionQueueEntry {
    pub fn new(comp: super::cmds::Completion, cid: u16) -> Self {
        Self {
            dw0: comp.dw0,
            rsvd: 0,
            sqhd: 0,
            sqid: 0,
            cid,
            status_phase: comp.status,
        }
    }
    pub fn set_phase(&mut self, phase: bool) {
        match phase {
            true => self.status_phase |= 0b1,
            false => self.status_phase &= !0b1,
        }
    }
}

// Register bits

bitstruct! {
    /// Representation of the Controller Capabilities (CAP) register.
    ///
    /// See NVMe 1.0e Section 3.1.1 Offset 00h: CAP - Controller Capabilities
    #[derive(Clone, Copy, Debug, Default)]
    pub struct Capabilities(pub u64) {
        /// Maximum Queue Entries Supported (MQES)
        ///
        /// The maximum individual queue size that the controller supports.
        /// This is a 0's based value and the minimum value is 1 (indicating a
        /// max size of 2).
        pub mqes: u16 = 0..16;

        /// Contiguous Queues Required  (CQR)
        ///
        /// Whether or not the controller requires I/O Completion/Submission
        /// Queues to be physically contiguous.
        pub cqr: bool = 16;

        /// Arbitration Mechanism Supported (AMS)
        ///
        /// Whether or not the controller supports Weighted Round Robin with Urgent.
        pub ams_roundrobin: bool = 17;

        /// Arbitration Mechanism Supported (AMS)
        ///
        /// Whether or not the controller supports a vendor specific arbitration mechanism.
        pub ams_vendor: bool = 18;

        /// Reserved
        reserved1: u8 = 19..24;

        /// Timeout (TO)
        ///
        /// The worst case time that host software shall wait for the controller to become ready.
        /// Specified as TO * 500ms
        pub to: u8 = 24..32;

        /// Doorbell Stride (DSTRD)
        ///
        /// Size between each completion/submission queue doorbell. Specified as 2^(2 + DSTRD) bytes.
        pub dstrd: u8 = 32..36;

        /// Reserved
        reserved2: u8 = 36;

        /// Command Sets Supported (CSS)
        ///
        /// Whether or not the controller supports NVM I/O command set.
        pub css_nvm: bool = 37;

        /// Command Sets Supported (CSS)
        ///
        /// Reserved bits for indicating other supported I/O command sets.
        css_reserved: u8 = 38..45;

        /// Reserved
        reserved3: u8 = 45..48;

        /// Memory Page Size Minimum (MPSMIN)
        ///
        /// The minimum host memory page size the controller supports.
        /// Specified as 2^(12 + MPSMIN) bytes.
        pub mpsmin: u8 = 48..52;

        /// Memory Page Size Maximum (MPSMAX)
        ///
        /// The maximum host memory page size the controller supports.
        /// Specified as 2^(12 + MPSMAX) bytes.
        pub mpsmax: u8 = 52..56;

        /// Reserved
        reserved4: u8 = 56..64;
    }
}
impl Capabilities {
    /// Size in bytes represented by the MPSMIN value
    pub fn mpsmin_sz(&self) -> usize {
        1 << (12 + self.mpsmin())
    }
}

bitstruct! {
    /// Representation of the Controller Configuration (CC) register.
    ///
    /// See NVMe 1.0e Section 3.1.5 Offset 14h: CC - Controller Configuration
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    pub struct Configuration(pub u32) {
        /// Enable (EN)
        ///
        /// When set to 1, the controller shall begin to process commands based
        /// on Submission Queue Tail Doorbell writes. When cleared to 0, the
        /// controller shall not process commands nor post completion queue
        /// entries. Transitioning from 1 to 0 indicates a Controller Reset.
        pub enabled: bool = 0;

        /// Reserved
        reserved1: u8 = 1..4;

        /// I/O Command Set Selected (CSS)
        ///
        /// The I/O Command Set selected by the host. Must be a supported
        /// command set as indicated by CAP.CSS. This field shall only be
        /// changed when the controller is disabled.
        pub css: IOCommandSet = 4..7;

        /// Memory Page Size (MPS)
        ///
        /// The host memory page size, respecting CAP.MPSMIN/MAX.
        /// Specified as 2^(12 + MPS) bytes.
        pub mps: u8 = 7..11;

        /// Arbitration Mechanism Selected (AMS)
        ///
        /// The Arbitration Mechanism selected by the host. Must be a supportedd
        /// mechanism as indicated by CAP.AMS. This field shall only be changed
        /// when the controller is disabled.
        pub ams: ArbitrationMechanism = 11..14;

        /// Shutdown Notification (SHN)
        ///
        /// Host writes to this field to indicate shutdown processing.
        pub shn: ShutdownNotification = 14..16;

        /// I/O Submission Queue Entry Size (IOSQES)
        ///
        /// Defines the I/O Submission Queue Entry size.
        /// Specified as 2^IOSQES bytes.
        pub iosqes: u8 = 16..20;

        /// I/O Completion Queue Entry Size (IOCQES)
        ///
        /// Defines the I/O Completion Queue Entry size.
        /// Specified as 2^IOCQES bytes.
        pub iocqes: u8 = 20..24;

        /// Reserved
        reserved2: u8 = 24..32;
    }
}

// Selected IO Command Set
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IOCommandSet {
    Nvm,
    Reserved(u8),
}

impl bitstruct::FromRaw<u8, IOCommandSet> for Configuration {
    fn from_raw(raw: u8) -> IOCommandSet {
        match raw {
            0b000 => IOCommandSet::Nvm,
            0b001..=0b111 => IOCommandSet::Reserved(raw),
            _ => unreachable!(),
        }
    }
}

impl bitstruct::IntoRaw<u8, IOCommandSet> for Configuration {
    fn into_raw(target: IOCommandSet) -> u8 {
        match target {
            IOCommandSet::Nvm => 0b000,
            IOCommandSet::Reserved(raw) => raw,
        }
    }
}

/// Arbitration Mechanisms
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArbitrationMechanism {
    RoundRobin,
    WeightedRoundRobinWithUrgent,
    Reserved(u8),
    Vendor,
}

impl bitstruct::FromRaw<u8, ArbitrationMechanism> for Configuration {
    fn from_raw(raw: u8) -> ArbitrationMechanism {
        match raw {
            0b000 => ArbitrationMechanism::RoundRobin,
            0b001 => ArbitrationMechanism::WeightedRoundRobinWithUrgent,
            0b010..=0b110 => ArbitrationMechanism::Reserved(raw),
            0b111 => ArbitrationMechanism::Vendor,
            _ => unreachable!(),
        }
    }
}

impl bitstruct::IntoRaw<u8, ArbitrationMechanism> for Configuration {
    fn into_raw(target: ArbitrationMechanism) -> u8 {
        match target {
            ArbitrationMechanism::RoundRobin => 0b000,
            ArbitrationMechanism::WeightedRoundRobinWithUrgent => 0b001,
            ArbitrationMechanism::Reserved(raw) => raw,
            ArbitrationMechanism::Vendor => 0b111,
        }
    }
}

/// Shutdown Notification Values
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ShutdownNotification {
    None,
    Normal,
    Abrupt,
    Reserved,
}

impl bitstruct::FromRaw<u8, ShutdownNotification> for Configuration {
    fn from_raw(raw: u8) -> ShutdownNotification {
        match raw {
            0b00 => ShutdownNotification::None,
            0b01 => ShutdownNotification::Normal,
            0b10 => ShutdownNotification::Abrupt,
            0b11 => ShutdownNotification::Reserved,
            _ => unreachable!(),
        }
    }
}

impl bitstruct::IntoRaw<u8, ShutdownNotification> for Configuration {
    fn into_raw(target: ShutdownNotification) -> u8 {
        match target {
            ShutdownNotification::None => 0b00,
            ShutdownNotification::Normal => 0b01,
            ShutdownNotification::Abrupt => 0b10,
            ShutdownNotification::Reserved => 0b11,
        }
    }
}

bitstruct! {
    /// Representation of the Controller Status (CSTS) register.
    ///
    /// See NVMe 1.0e Section 3.1.6 Offset 1Ch: CSTS - Controller Status
    #[derive(Clone, Copy, Debug, Default)]
    pub struct Status(pub u32) {
        /// Ready (RDY)
        ///
        /// Controller sets this field to 1 to indicate it is ready to accept
        /// Submission Queue Tail Doorbell writes.
        pub ready: bool = 0;

        /// Controller Fatal Status (CFS)
        ///
        /// Controller sets this field to 1 when a fatal error occurs.
        pub cfs: bool = 1;

        /// Shutdown Status (SHST)
        ///
        /// Indicates the current shutdown processing state.
        pub shst: ShutdownStatus = 2..4;

        /// Reserved
        reserved: u32 = 4..32;
    }
}

/// Shutdown Status
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ShutdownStatus {
    Normal,
    Processing,
    Complete,
    Reserved,
}

impl bitstruct::FromRaw<u8, ShutdownStatus> for Status {
    fn from_raw(raw: u8) -> ShutdownStatus {
        match raw {
            0b00 => ShutdownStatus::Normal,
            0b01 => ShutdownStatus::Processing,
            0b10 => ShutdownStatus::Complete,
            0b11 => ShutdownStatus::Reserved,
            _ => unreachable!(),
        }
    }
}

impl bitstruct::IntoRaw<u8, ShutdownStatus> for Status {
    fn into_raw(target: ShutdownStatus) -> u8 {
        match target {
            ShutdownStatus::Normal => 0b00,
            ShutdownStatus::Processing => 0b01,
            ShutdownStatus::Complete => 0b10,
            ShutdownStatus::Reserved => 0b11,
        }
    }
}

bitstruct! {
    /// Representation of the Admin Queue Attributes (AQA) register.
    ///
    /// See NVMe 1.0e Section 3.1.7 Offset 24h: AQA - Admin Queue Attributes
    #[derive(Clone, Copy, Debug, Default)]
    pub struct AdminQueueAttrs(pub u32) {
        /// Admin Submission Queue Size (ASQS)
        ///
        /// Defines the size of the Admin Submission Queue as a 0's
        /// based value.
        pub asqs: u16 = 0..12;

        /// Reserved
        reserved1: u8 = 12..16;

        /// Admin Completion Queue Size (ACQS)
        ///
        /// Defines the size of the Admin Completion Queue as a 0's
        /// based value.
        pub acqs: u16 = 16..28;

        /// Reserved
        reserved2: u8 = 28..32;
    }
}

// Version definitions

/// Controller Version NVM Express 1.0
///
/// Bits 31:16  Major Version Number (MJR) = "1"
/// Bits 15:00  Minor Version Number (MNR) = "0"
///
/// See NVMe 1.0e Section 3.1.2 Offset 08h: VS - Version
pub const NVME_VER_1_0: u32 = 0x00010000;

// Admin Command Opcodes
// See NVMe 1.0e Section 5, Figure 25 Opcodes for Admin Commands

/// Delete I/O Submission Queue Command Opcode
pub const ADMIN_OPC_DELETE_IO_SQ: u8 = 0x00;
/// Create I/O Submission Queue Command Opcode
pub const ADMIN_OPC_CREATE_IO_SQ: u8 = 0x01;
/// Get Log Page Command Opcode
pub const ADMIN_OPC_GET_LOG_PAGE: u8 = 0x02;
/// Delete I/O Completion Queue Command Opcode
pub const ADMIN_OPC_DELETE_IO_CQ: u8 = 0x04;
/// Create I/O Completion Queue Command Opcode
pub const ADMIN_OPC_CREATE_IO_CQ: u8 = 0x05;
/// Identify Command Opcode
pub const ADMIN_OPC_IDENTIFY: u8 = 0x06;
/// Abort Command Opcode
pub const ADMIN_OPC_ABORT: u8 = 0x08;
/// Set Feature Command Opcode
pub const ADMIN_OPC_SET_FEATURES: u8 = 0x09;
/// Get Feature Command Opcode
pub const ADMIN_OPC_GET_FEATURES: u8 = 0x0A;
/// Asynchronous Event Request Command Opcode
pub const ADMIN_OPC_ASYNC_EVENT_REQ: u8 = 0x0c;
/// Doorbell Buffer Config
pub const ADMIN_OPC_DOORBELL_BUF_CFG: u8 = 0x7c;

// NVM Command Opcodes
// See NVMe 1.0e Section 6, Figure 99 Opcodes for NVM Commands

/// Flush Command Opcode
pub const NVM_OPC_FLUSH: u8 = 0x00;
/// Write Command Opcode
pub const NVM_OPC_WRITE: u8 = 0x01;
/// Read Command Opcode
pub const NVM_OPC_READ: u8 = 0x02;

// Generic Command Status values
// See NVMe 1.0e Section 4.5.1.2.1, Figure 17 Status Code - Generic Command Status Values

/// Successful Completion
///
/// The command completed successfully.
pub const STS_SUCCESS: u8 = 0x0;

/// Invalid Command Opcode
///
/// The associated command opcode field is not valid.
pub const STS_INVAL_OPC: u8 = 0x1;

/// Invalid Field in Command
///
/// An invalid field specified in the command parameters.
pub const STS_INVAL_FIELD: u8 = 0x2;

/// Command ID Conflict
///
/// The command identifier is already in use.
pub const STS_CID_CONFLICT: u8 = 0x3;

/// Data Transfer Error
///
/// Transferring the data or metadata associated with a command had an error.
pub const STS_DATA_XFER_ERR: u8 = 0x4;

/// Commands Aborted due to Power Loss Notification
///
/// Indicates that the command was aborted due to a power loss notifiation.
pub const STS_PWR_LOSS_ABRT: u8 = 0x5;

/// Internal Device Error
///
/// The command was not completed successfully due to an internal device error.
pub const STS_INTERNAL_ERR: u8 = 0x6;

/// Command Abort Requested
///
/// The command was aborted due to a Command Abort command.
pub const STS_ABORT_REQ: u8 = 0x7;

/// Command Aborted due to SQ Deletion
///
/// The command was aborted due to a Delete I/O Submission Queue request.
pub const STS_ABORT_SQ_DEL: u8 = 0x8;

/// Command Aborted due to Failed Fused Command
///
/// The command was aborted due to the other command in a fused command failing.
pub const STS_FAILED_FUSED: u8 = 0x9;

/// Command Aborted due to Missing Fused Command
///
/// The command was aborted due to the companion fused command not being found.
pub const STS_MISSING_FUSED: u8 = 0xA;

/// Invalid Namespace or Format
///
/// The namespace or the format of that namespace is invalid.
pub const STS_INVALID_NS: u8 = 0xB;

/// Command Sequence Error
///
/// The command was aborted due to a protocol violation in a multi-command sequence.
pub const STS_COMMAND_SEQ_ERR: u8 = 0xC;

// Command Specific Status values
// See NVMe 1.0e Section 4.5.1.2.2, Figure 19 Status Code - Command Specific Status Values

/// Completion Queue Invalid
pub const STS_CREATE_IO_Q_INVAL_CQ: u8 = 0x0;

/// Invalid Queue Identifier (Queue Creation)
pub const STS_CREATE_IO_Q_INVAL_QID: u8 = 0x1;

/// Invalid Queue Size (Queue Creation)
pub const STS_CREATE_IO_Q_INVAL_QSIZE: u8 = 0x2;

/// Invalid Interrupt Vector (Queue Creation)
pub const STS_CREATE_IO_Q_INVAL_INT_VEC: u8 = 0x8;

/// Invalid Queue Identifier (Queue Deletion)
pub const STS_DELETE_IO_Q_INVAL_QID: u8 = 0x1;

/// Invalid Queue Deletion
pub const STS_DELETE_IO_Q_INVAL_Q_DELETION: u8 = 0xC;

// NVM Command Specific Status values
// See NVMe 1.0e Section 4.5.1.2.2, Figure 20 Status Code - Command Specific Status Values, NVM Command Set

/// Conflicting Attributes
pub const STS_READ_CONFLICTING_ATTRS: u8 = 0x80;

/// Invalid Protection Information
pub const STS_READ_INVALID_PROT_INFO: u8 = 0x81;

/// Attempted to Write Read Only Range
pub const STS_WRITE_READ_ONLY_RANGE: u8 = 0x82;

// Feature identifiers
// See NVMe 1.0e Section 5.12.1, Figure 73 Set Features - Feature Identifiers

/// Arbitration
///
/// See NVMe 1.0e Section 5.12.1.1 Arbitration (Feature Identifier 01h)
pub const FEAT_ID_ARBITRATION: u8 = 0x01;

/// Power Management
///
/// See NVMe 1.0e Section 5.12.1.2 Power Management (Feature Identifier 02h)
pub const FEAT_ID_POWER_MGMT: u8 = 0x02;

/// LBA Range Type
///
/// See NVMe 1.0e Section 5.12.1.3 LBA Range Type (Feature Identifier 03h)
pub const FEAT_ID_LBA_RANGE_TYPE: u8 = 0x03;

/// Temperature Threshold
///
/// See NVMe 1.0e Section 5.12.1.4 Arbitration (Feature Identifier 04h)
pub const FEAT_ID_TEMP_THRESH: u8 = 0x04;

/// Error Recovery
///
/// See NVMe 1.0e Section 5.12.1.5 Error Recovery (Feature Identifier 05h)
pub const FEAT_ID_ERROR_RECOVERY: u8 = 0x05;

/// Volatile Write Cache
///
/// See NVMe 1.0e Section 5.12.1.6 Volatile Write Cache (Feature Identifier 06h)
pub const FEAT_ID_VOLATILE_WRITE_CACHE: u8 = 0x06;

/// Number of Queues
///
/// See NVMe 1.0e Section 5.12.1.7 Number of Queues (Feature Identifier 07h)
pub const FEAT_ID_NUM_QUEUES: u8 = 0x07;

/// Interrupt Coalescing
///
/// See NVMe 1.0e Section 5.12.1.8 Interrupt Coalescing (Feature Identifier 08h)
pub const FEAT_ID_INTR_COALESCE: u8 = 0x08;

/// Interrupt Vector Configuration
///
/// See NVMe 1.0e Section 5.12.1.9 Interrupt Vector Configuration (Feature Identifier 09h)
pub const FEAT_ID_INTR_VEC_CFG: u8 = 0x09;

/// Write Atomicity
///
/// See NVMe 1.0e Section 5.12.1.10 Write Atomicity (Feature Identifier 0Ah)
pub const FEAT_ID_WRITE_ATOMIC: u8 = 0x0A;

/// Asynchronous Event Configuration
///
/// See NVMe 1.0e Section 5.12.1.11 Asynchronous Event Configuration (Feature Identifier 0Bh)
pub const FEAT_ID_ASYNC_EVENT_CFG: u8 = 0x0B;

// Identify CNS values

/// Identify - Namespace Structure
///
/// Return the Identify Namespace data structure in response to Identify command.
/// See NVMe 1.0e Section 5.11
pub const IDENT_CNS_NAMESPACE: u8 = 0x0;

/// Identify - Controller Structure
///
/// Return the Identify Controller data structure in response to Identify command.
/// See NVMe 1.0e Section 5.11
pub const IDENT_CNS_CONTROLLER: u8 = 0x1;

/// The type of value specified in the Status Field (SF) of a command completion.
///
/// See NVMe 1.0e Section 4.5.1.1 Status Code Type (SCT)
#[derive(Copy, Clone, Debug, PartialEq)]
#[repr(u8)]
pub enum StatusCodeType {
    Generic = 0,
    CmdSpecific = 1,
    MediaDataIntegrity = 2,
    VendorSpecific = 7,
}

/// Power State Descriptor (PSD) Data Structure
///
/// Describes the characteristics of a specific power state.
///
/// See NVMe 1.0e Section 5.11, Figure 67 Identify - Power State Descriptor Data Structure
#[derive(Default, Copy, Clone)]
#[repr(C, packed(1))]
pub struct PowerStateDescriptor {
    /// Maximum Power
    ///
    /// Maximum power consumed by NVM subsystem in this power state.
    /// The value multiplied by 0.01 is equal to the power in Watts.
    pub mp: u16,

    /// Reserved - Bits 31:16
    pub _resv1: u16,

    /// Entry Latency (ENLAT)
    ///
    /// The maximum entry latency in microseconds.
    pub enlat: u32,

    /// Exit Latency (EXLAT)
    ///
    /// The maximum exit latency in microseconds.
    pub exlat: u32,

    /// Relative Read Throughput (RRT)
    ///
    /// Must be less than the number of supported power states.
    ///
    /// Top 3 bits are reserved - Bits 103:101
    pub rrt: u8,

    /// Relative Read Latency (RRL)
    ///
    /// Must be less than the number of supported power states.
    ///
    /// Top 3 bits are reserved - Bits 111:109
    pub rrl: u8,

    /// Relative Write Throughput (RWT)
    ///
    /// Must be less than the number of supported power states.
    ///
    /// Top 3 bits are reserved - Bits 119:117
    pub rwt: u8,

    /// Relative Write Latency (RWL)
    ///
    /// Must be less than the number of supported power states.
    ///
    /// Top 3 bits are reserved - Bits 127:125
    pub rwl: u8,

    /// Reserved - Bits 255:128
    pub _resv: [u8; 16],
}

bitstruct! {
    /// Queue Entry Size Required & Maximum (both Completion & Submission)
    ///
    /// Defines the required and maximum Queue entry sizes when using the NVM Command Set.
    #[derive(Copy, Clone)]
    pub struct NvmQueueEntrySize(pub u8) {
        /// The required (minimum) Queue Entry Size.
        ///
        /// Specified as 2^required bytes. It shall be 6 (64 bytes).
        pub required: u8 = 0..4;

        /// The maximum Queue Entry Size and is >= the required size.
        ///
        /// Specified as 2^required bytes.
        /// The recommended maximum is 6 (64 bytes) for standad NVM Command Set.
        /// Controllers with proprietary extensions may support a larger size.
        pub maximum: u8 = 4..8;
    }
}

/// Identify Controller Data Structure
///
/// Describes the characteristics of the controller.
///
/// See NVMe 1.0e Section 5.11, Figure 66 Identify - Identify Controller Data Structure
#[derive(Copy, Clone)]
#[repr(C, packed(1))]
pub struct IdentifyController {
    // bytes 0-255 - Controller Capabilities and Features
    /// PCI Vendor ID (VID)
    ///
    /// Same value reported in ID register.
    /// See NVMe 1.0e Section 2.1.1 Offset 00h: ID - Identifiers
    pub vid: u16,
    /// PCI Subsystem Vendor ID (SSVID)
    ///
    /// Same value reported in SS register.
    /// See NVMe 1.0e Section 2.1.17 Offset 2Ch: ID - Sub System Identifiers
    pub ssvid: u16,
    /// Serial Number (SN)
    ///
    /// See NVMe 1.0e Section 7.7 Unique Identifier
    pub sn: [u8; 20],
    /// Model Number (MN)
    ///
    /// See NVMe 1.0e Section 7.7 Unique Identifier
    pub mn: [u8; 40],
    /// Firmware Revision (FR)
    ///
    /// Same revision information returned via Get Log Page command.
    /// See NVMe 1.0e Section 5.10.1.3 Firmware Slot Information (Log Identifier 03h)
    pub fr: [u8; 8],
    /// Recommended Arbitration Burst (RAB)
    ///
    /// See NVMe 1.0e Section 4.7 Command Arbitration
    pub rab: u8,
    /// IEEE OUI Identifier (IEEE)
    pub ieee: [u8; 3],
    /// Multi-Interface Capabilities (MIC)
    ///
    /// Whether there are multiple physical PCIe interfaces and associated capabilities.
    /// Bits 7:1 are optional.
    pub cmic: u8,
    /// Maximum Data Transfer Size (MDTS)
    ///
    /// The host (VM) should not submit a command that exceeds this transfer size.
    /// The value is in unites of the minimum memory page size (CAP.MPSMIN) and is
    /// reported as a power of two (2^n). A value of 0h indicates no restrictions on
    /// transfer size. The restrictions includes interleaved metadata.
    pub mdts: u8,
    /// Reserved - Bytes 255:78
    pub _resv1: [u8; 178],

    // bytes 256-511 - Admin Command Set Attributes & Optional Controller Capabilities
    /// Optional Admin Command Support (OACS)
    ///
    /// Bits 15:3 are reserved.
    /// Bit 2 indicates Firmware Activate & Download command support.
    /// Bit 1 indicates Format NVM command support.
    /// Bit 0 indicates Security Send/Receive command support.
    pub oacs: u16,
    /// Abort Command Limit (ACL)
    ///
    /// Maximum number of concurrently outstanding Abort commands supported.
    /// This is a 0's based value.
    /// See NVMe 1.0e Section 5.1 Abort command
    pub acl: u8,
    /// Asynchronous Event Request Limit (AERL)
    ///
    /// Maximum number of concurrently outstanding Asynchronous Event Request commands
    /// supported.
    /// This is a 0's based value.
    /// See NVMe 1.0e Section 5.2 Asynchronous Event Request command
    pub aerl: u8,
    /// Firmware Updates (FRMW)
    ///
    /// Bits 7:4 are reserved.
    /// Bits 3:1 indicate number of firmware slots device supports (between 1-7, inclusive)
    /// Bit 0 indicates if the first firmware slot (slot 1) is read-only.
    /// See NVMe 1.0e Section 8.1 Firmware Update Process
    pub frmw: u8,
    /// Log Page Attributes (LPA)
    ///
    /// Bits 7:1 are reserved.
    /// Bit 0 indicated per-namespace SMART/Health information log support.
    pub lpa: u8,
    /// Error Log Page Entries (ELPE)
    ///
    /// Number of Error Information log entries that are stored by the controller.
    /// This is a 0's based value.
    pub elpe: u8,
    /// Number of Power States Support (NPSS)
    ///
    /// Number of NVMe power states supported by the controller (up to 32 total).
    /// This is a 0's based value, i.e. 1 minimum.
    /// See NVMe 1.0e Section 8.4 Power Management
    pub npss: u8,
    /// Admin Vendor Specific Command Configuration (AVSCC)
    ///
    /// Bits 7:1 are reserved.
    /// Bit 0 indicates that all Admin Vendor Specific Commands use format in Figure 8.
    /// See NVMe 1.0e Section 4.2, Figure 8 Command Format - Admin and NVM Vendor Specific Commands (Optional)
    pub avscc: u8,
    /// Reserved
    pub _resv2: [u8; 247],

    // bytes 512-2047 - NVM Command Set Attributes
    /// Submission Queue Entry Size (SQES)
    ///
    /// Defines the required and maximum Submission Queue entry sizes when using the NVM Command Set.
    /// Bits 7:4 define the maximum SQES and is >= the required SQES.
    /// Bits 3:0 define the required (minimum) SQES. It shall be 6 (64 bytes).
    ///
    /// The recommended maximum SQES is 6 (64 bytes) for standard NVM Command Set.
    /// Controllers with proprietary extensions may support a larger size.
    /// Both the required and maximum SQES values are in bytes and reported as powers of two (2^n).
    pub sqes: NvmQueueEntrySize,
    /// Completion Queue Entry Size (CQES)
    ///
    /// Defines the required and maximum Completion Queue entry sizes when using the NVM Command Set.
    /// Bits 7:4 define the maximum CQES and is >= the required CQES.
    /// Bits 3:0 define the required (minimum) CQES. It shall be 4 (16 bytes).
    ///
    /// The recommended maximum CQES is 4 (16 bytes) for standard NVM Command Set.
    /// Controllers with proprietary extensions may support a larger size.
    /// Both the required and maximum CQES values are in bytes and reported as powers of two (2^n).
    pub cqes: NvmQueueEntrySize,
    /// Reserved - Bytes 515:514
    pub _resv3: [u8; 2],
    /// Number of Namespaces (NN)
    ///
    /// The number of valid namespaces present for the controller. Namespaces shall start
    /// with namespace ID 1 and be packed sequentially.
    pub nn: u32,
    /// Option NVM Command Support (ONCS)
    ///
    /// Bits 15:3 are reserved.
    /// Bit 2 indicates Dataset Management command support.
    /// Bit 1 indicates Write Uncorrectable command support.
    /// Bit 0 indicates Compare command support.
    pub oncs: u16,
    /// Fused Operation Support (FUSES)
    ///
    /// Bits 15:1 are reserved.
    /// Bit 0 indicates Compare and Write fused operation support.
    pub fuses: u16,
    /// Format NVM Attributes (FNA)
    ///
    /// Bits 7:3 are reserved.
    /// Bit 2 indicates cryptographic erase support.
    /// Bit 1 indicates whether secure erase is a global (1) or per-namespace (0) operation.
    /// Bit 0 indicates whether format is a global (1) or per-namespace (0) operation.
    pub fna: u8,
    /// Volatile Write Cache (VWC)
    ///
    /// Bits 7:1 are reserved.
    /// Bit 0 indicates whether a volatile write cache is present.
    pub vwc: u8,
    /// Atomic Write Unit Normal (AWUN)
    ///
    /// Indicates the atomic write size for the controller during normal operation.
    /// This field is specified in logical blocks and is a 0's based value.
    pub awun: u16,
    /// Atomic Write Unit Power Fail (AWUPF)
    ///
    /// Indicates the atomic write size for the controller during a power fail condition.
    /// This field is specified in logical blocks and is a 0's based value.
    pub awupf: u16,
    /// NVM Vendor Specific Command Configuration (NVSCC)
    ///
    /// Bits 7:1 are reserved
    /// Bit 0 indicates that all NVM Vendor Specific Commands use format in Figure 8.
    /// See NVMe 1.0e Section 4.2, Figure 8 Command Format - Admin and NVM Vendor Specific Commands (Optional)
    /// See NVMe 1.0e Section 8.7 Standard Vendor Specific Command Format
    pub nvscc: u8,
    /// Reserved - Bytes 703:531
    pub _resv4: [u8; 173],
    /// Reserved (I/O Command Set Attributes) - Bytes 2047:704
    pub _resv5: [u8; 1344],

    // bytes 2048-3071 - Power State Descriptors
    /// Power State Descriptors (PSD0-PSD31)
    pub psd: [PowerStateDescriptor; 32],

    // bytes 3072-4095 - Vendor Specific
    /// Vendor Specific (VS)
    pub vs: [u8; 1024],
}

// We can't derive Default since Default isn't impl'd
// for [T; N] where N > 32 yet (rust #61415)
impl Default for IdentifyController {
    fn default() -> Self {
        Self {
            vid: 0,
            ssvid: 0,
            sn: [0; 20],
            mn: [0; 40],
            fr: [0; 8],
            rab: 0,
            ieee: [0; 3],
            cmic: 0,
            mdts: 0,
            oacs: 0,
            acl: 0,
            aerl: 0,
            frmw: 0,
            lpa: 0,
            elpe: 0,
            npss: 0,
            avscc: 0,
            sqes: NvmQueueEntrySize(0),
            cqes: NvmQueueEntrySize(0),
            nn: 0,
            oncs: 0,
            fuses: 0,
            fna: 0,
            vwc: 0,
            awun: 0,
            awupf: 0,
            nvscc: 0,
            psd: [PowerStateDescriptor::default(); 32],
            vs: [0; 1024],

            _resv1: [0; 178],
            _resv2: [0; 247],
            _resv3: [0; 2],
            _resv4: [0; 173],
            _resv5: [0; 1344],
        }
    }
}

/// LBA Format Data Structure
///
/// Describes a specific Logical Block Address (LBA) format.
/// See NVMe 1.0e Section 5.11, Figure 69 Identify - LBA Format Data Structure, NVM Command Set Specific
#[derive(Default, Copy, Clone)]
#[repr(C, packed(1))]
pub struct LbaFormat {
    /// Metadata Size (MS)
    ///
    /// The number of metadata bytes provided per LBA.
    pub ms: u16,
    /// LBA Data Size (LBADS)
    ///
    /// The LBA data size supported and reported in terms of power of two (2^n).
    /// The minimum required value is 9 (512 bytes).
    /// If the value is 0h, then the LBA format is not supported.
    pub lbads: u8,
    /// Relative Performance (RP)
    ///
    /// Bits 7:2 are reserved.
    /// Bits 1:0 indicate the performance of this LBA format relative to other supported LBA formats.
    ///     00b = Best Performance
    ///     01b = Better Performance
    ///     10b = Good Performance
    ///     11b = Degraded Performance
    pub rp: u8,
}

/// Identify Namespace Data Structure
///
/// Describes the characteristics of a namespace.
///
/// See NVMe 1.0e Section 5.11, Figure 68 Identify - Identify Namespace Data Structure, NVM Command Set Specific
#[derive(Copy, Clone)]
#[repr(C, packed(1))]
pub struct IdentifyNamespace {
    /// Namespace Size (NSZE)
    ///
    /// The total size of the namespace in logical blocks. A namespace of size
    /// n consists of Logical Block Addresses (LBA) 0 through n-1.
    pub nsze: u64,
    /// Namespace Capacity (NCAP)
    ///
    /// NCAP <= NSZE.
    /// The maximum number of logical blocks that may be allocated in the namespace
    /// at any point in time. This field is used in the case of thin provisioning.
    /// A logical block is allocated when written with a Write (Uncorrectable) command.
    /// A value of 0h indicates that the namespace is not available for use.
    pub ncap: u64,
    /// Namespace Utilization (NUSE)
    ///
    /// NUSE <= NCAP
    /// The current number of logical blocks allocated in the namespace.
    pub nuse: u64,
    /// Namespace Features (NSFEAT)
    ///
    /// Bits 7:1 are reserved.
    /// Bit 0 indicates whether the namespace supports thin provisioning.
    pub nsfeat: u8,
    /// Number of LBA Formats (NLBAF)
    ///
    /// The number of supported LBA data size and metadata size combinations.
    /// LBA formats shall be allocated (starting with 0) and packed sequentially.
    /// This is a 0's based value.
    /// The maximum number of LBA formats that may be indicated as supported is 16.
    /// The supported LBA formats are defined in the `lbaf` field.
    pub nlbaf: u8,
    /// Formatted LBA Size (FLBAS)
    ///
    /// Bits 7:5 are reserved.
    /// Bit 4 indicates that the metadata is transferred at the end of the data LBA.
    /// Bits 3:0 indicate one of the 16 supported combinations in `lbaf`.
    pub flbas: u8,
    /// Metadata Capabilities (MC)
    ///
    /// Bits 7:2 are reserved
    /// Bit 1 indicated whether namespace supports transferring maetadata in a separate buffer.
    /// Bit 0 indicates whether namespace supports transferring metadata as part of extended data LBA.
    pub mc: u8,
    /// End-to-end Data Protection Capabilities (DPC)
    ///
    /// Bits 7:5 are reserved.
    /// Bit 4 indicates whether namespace supports protection information in last 8 bytes of metadata.
    /// Bit 3 indicates whether namespace supports protection information in first 8 bytes of metadata.
    /// Bit 2 indicates whether namespace supports Protection Information Type 3.
    /// Bit 1 indicates whether namespace supports Protection Information Type 2.
    /// Bit 0 indicates whether namespace supports Protection Information Type 1.
    /// See NVMe 1.0e Section 8.3 End-to-end Data Protection (Optional)
    pub dpc: u8,
    /// End-to-end Data Protection Type Settings (DPS)
    ///
    /// Bits 7:4 are reserved
    /// Bit 3 indicates that the protection information, if enabled, is transferred as first 8 bytes of metadata (1) or last 8 bytes (0).
    /// Bits 2:0 indicate whether Protection Information is enabled and the type.
    ///     000b = Protection information is not enabled
    ///     001b = Protection information is enabled, Type 1
    ///     010b = Protection information is enabled, Type 2
    ///     011b = Protection information is enabled, Type 3
    ///     100b-111b = Reserved
    /// See NVMe 1.0e Section 8.3 End-to-end Data Protection (Optional)
    pub dps: u8,
    /// Reserved - Bytes 127:30
    pub _resv1: [u8; 98],
    /// LBA Formats (LBAF0-LBAF15)
    ///
    /// The list of supported LBA formats.
    pub lbaf: [LbaFormat; 16],
    /// Reserved - Bytes 383:192
    pub _resv2: [u8; 192],
    /// Vendor Specific (VS)
    pub vs: [u8; 3712],
}

// We can't derive Default since Default isn't impl'd
// for [T; N] where N > 32 yet (rust #61415)
impl Default for IdentifyNamespace {
    fn default() -> Self {
        Self {
            nsze: 0,
            ncap: 0,
            nuse: 0,
            nsfeat: 0,
            nlbaf: 0,
            flbas: 0,
            mc: 0,
            dpc: 0,
            dps: 0,
            lbaf: [LbaFormat::default(); 16],
            vs: [0; 3712],

            _resv1: [0; 98],
            _resv2: [0; 192],
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use std::mem::size_of;

    #[test]
    fn entry_sizing() {
        assert_eq!(size_of::<SubmissionQueueEntry>(), 64);
        assert_eq!(size_of::<CompletionQueueEntry>(), 16);
        assert_eq!(size_of::<PowerStateDescriptor>(), 32);
        assert_eq!(size_of::<IdentifyController>(), 4096);
        assert_eq!(size_of::<LbaFormat>(), 4);
        assert_eq!(size_of::<IdentifyNamespace>(), 4096);
    }
}
