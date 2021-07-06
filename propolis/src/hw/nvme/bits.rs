#![allow(unused)]

#[derive(Debug, Default, Copy, Clone)]
#[repr(C)]
pub struct RawSubmission {
    pub cdw0: u32,
    pub nsid: u32,
    pub rsvd: u64,
    pub mptr: u64,
    pub prp1: u64,
    pub prp2: u64,
    pub cdw10: u32,
    pub cdw11: u32,
    pub cdw12: u32,
    pub cdw13: u32,
    pub cdw14: u32,
    pub cdw15: u32,
}
impl RawSubmission {
    pub fn cid(&self) -> u16 {
        (self.cdw0 >> 16) as u16
    }
    pub fn opcode(&self) -> u8 {
        self.cdw0 as u8
    }
}

#[derive(Debug, Default, Copy, Clone)]
#[repr(C)]
pub struct RawCompletion {
    pub cdw0: u32,
    pub rsvd: u32,
    pub sqhd: u16,
    pub sqid: u16,
    pub cid: u16,
    pub status: u16,
}

// Register bits

pub const CAP_CCS: u64 = 1 << 37; // CAP.CCS - NVM command set
pub const CAP_CQR: u64 = 1 << 16; // CAP.CQR - require contiguous queus

pub const CC_EN: u32 = 0x1;

pub const CSTS_READY: u32 = 0x1;

// Version definitions

pub const NVME_VER_1_0: u32 = 0x00010000;

// Admin Command Opcodes

pub const ADMIN_OPC_DELETE_IO_SQ: u8 = 0x00;
pub const ADMIN_OPC_CREATE_IO_SQ: u8 = 0x01;
pub const ADMIN_OPC_GET_LOG_PAGE: u8 = 0x02;
pub const ADMIN_OPC_DELETE_IO_CQ: u8 = 0x04;
pub const ADMIN_OPC_CREATE_IO_CQ: u8 = 0x05;
pub const ADMIN_OPC_IDENTIFY: u8 = 0x06;
pub const ADMIN_OPC_ABORT: u8 = 0x08;
pub const ADMIN_OPC_SET_FEATURES: u8 = 0x09;
pub const ADMIN_OPC_GET_FEATURES: u8 = 0x0a;
pub const ADMIN_OPC_ASYNC_EVENT_REQ: u8 = 0x0c;

// Nvm Command Opcodes

pub const NVM_OPC_FLUSH: u8 = 0x00;
pub const NVM_OPC_WRITE: u8 = 0x01;
pub const NVM_OPC_READ: u8 = 0x02;

// Command Status values

pub const STS_SUCCESS: u8 = 0x0;
pub const STS_INVAL_OPC: u8 = 0x1;
pub const STS_INVAL_FIELD: u8 = 0x2;
pub const STS_CID_CONFLICT: u8 = 0x3;
pub const STS_DATA_XFER_ERR: u8 = 0x4;
pub const STS_PWR_LOSS_ABRT: u8 = 0x5;
pub const STS_INTERNAL_ERR: u8 = 0x6;
pub const STS_ABORT_REQ: u8 = 0x7;
pub const STS_ABORT_SQ_DEL: u8 = 0x8;
pub const STS_FAILED_FUSED: u8 = 0x9;
pub const STS_MISSING_FUSED: u8 = 0xa;
pub const STS_INVALID_NS: u8 = 0xb;
pub const STS_COMMAND_SEQ_ERR: u8 = 0xc;
pub const STS_INVAL_SGL_DESC: u8 = 0xd;
pub const STS_INVAL_SGL_NUM: u8 = 0xe;
pub const STS_INVAL_SGL_LEN: u8 = 0xf;
pub const STS_INVAL_SGL_META_LEN: u8 = 0x10;
pub const STS_INVAL_SGL_TYPE_LEN: u8 = 0x11;
pub const STS_INVAL_CMB_USE: u8 = 0x12;
pub const STS_INVAL_PRP_OFFSET: u8 = 0x13;

pub const STS_CREATE_IO_Q_INVAL_CQ: u8 = 0x0;
pub const STS_CREATE_IO_Q_INVAL_QID: u8 = 0x1;
pub const STS_CREATE_IO_Q_INVAL_QSIZE: u8 = 0x2;
pub const STS_CREATE_IO_Q_INVAL_INT_VEC: u8 = 0x8;

// SetFeature Command Specific Status values

pub const STS_SET_FEATURE_NOT_SAVEABLE: u8 = 0x0D;
pub const STS_SET_FEATURE_NOT_CHANGEABLE: u8 = 0x0E;
pub const STS_SET_FEATURE_NOT_NAMESPACE_SPECIFIC: u8 = 0x0F;
pub const STS_SET_FEATURE_OVERLAPPING_RANGES: u8 = 0x14;

// Read Command Status values

pub const STS_READ_CONFLICTING_ATTRS: u8 = 0x80;
pub const STS_READ_INVALID_PROT_INFO: u8 = 0x81;

// Feature identifiers

pub const FEAT_ID_ARBITRATION: u8 = 0x01;
pub const FEAT_ID_POWER_MGMT: u8 = 0x02;
pub const FEAT_ID_TEMP_THRESH: u8 = 0x04;
pub const FEAT_ID_ERROR_RECOVERY: u8 = 0x05;
pub const FEAT_ID_NUM_QUEUES: u8 = 0x07;
pub const FEAT_ID_INTR_COALESCE: u8 = 0x08;
pub const FEAT_ID_INTR_VEC_CFG: u8 = 0x09;
pub const FEAT_ID_WRITE_ATOMIC: u8 = 0x0a;
pub const FEAT_ID_ASYNC_EVENT_CFG: u8 = 0x0b;

// Identify CNS values

pub const IDENT_CNS_NAMESPACE: u8 = 0x0;
pub const IDENT_CNS_CONTROLLER: u8 = 0x1;
pub const IDENT_CNS_ACTIVE_NSID: u8 = 0x2;
pub const IDENT_CNS_NSID_DESC: u8 = 0x3;


#[derive(Copy, Clone)]
#[repr(u8)]
pub enum StatusCodeType {
    Generic = 0,
    CmdSpecific = 1,
    MediaDataIntegrity = 2,
    VendorSpecific = 7,
}

#[derive(Default, Copy, Clone)]
#[repr(C)]
pub struct PowerStateDescriptor {
    /// Maximum Power
    pub mp: u16,
    /// Reserved
    pub _resv1: u16,
    /// Entry Latency
    pub enlat: u32,
    /// Exit Latency
    pub exlat: u32,
    /// Relative Read Throughput
    pub rrt: u8,
    /// Relative Read Latency
    pub rrl: u8,
    /// Relative Write Throughput
    pub rwt: u8,
    /// Relative Write Latency
    pub rwl: u8,
    /// Reserved
    pub _resv: [u8; 16],
}

#[derive(Copy, Clone)]
#[repr(C)]
pub struct IdentifyController {
    // bytes 0-255 - Controller Capabilities and Features
    /// PCI Vendor ID
    pub vid: u16,
    /// PCI Subsystem Vendor ID
    pub ssvid: u16,
    /// Serial Number
    pub sn: [u8; 20],
    /// Model Number
    pub mn: [u8; 40],
    /// Firmware Revision
    pub fr: [u8; 8],
    /// Recommended Arbitration Burst
    pub rab: u8,
    /// IEEE OUI Identifier
    pub ieee: [u8; 3],
    /// Multi-Interface Capabilities
    pub cmic: u8,
    /// Maximum Data Transfer Size
    pub mdts: u8,
    /// Reserved
    pub _resv1: [u8; 178],

    // bytes 256-511 - Admin Command Set Attributes & Optional Controller Capabilities
    /// Optional Admin Command Support
    pub oacs: u16,
    /// Abort Command Limit
    pub acl: u8,
    /// Asynchronous Event Request Limit
    pub aerl: u8,
    /// Firmware Updates
    pub frmw: u8,
    /// Log Page Attributes
    pub lpa: u8,
    /// Error Log Page Etnries
    pub elpe: u8,
    /// Number of Power States Support
    pub npss: u8,
    /// Admin Vedor Specific Command Configuration
    pub avscc: u8,
    /// Reserved
    pub _resv2: [u8; 246],

    // bytes 512-2047 - NVM Command Set Attributes
    /// Submission Queue Entry Size
    pub sqes: u8,
    /// Completion Queue Entry Size
    pub cqes: u8,
    /// Reserved
    pub _resv3: [u8; 2],
    /// Number of Namespaces
    pub nn: u32,
    /// Option NVM Command Support
    pub oncs: u16,
    /// Fused Operation Support
    pub fuses: u16,
    /// Format NVM Attributes
    pub fna: u8,
    /// Volatile Write Cache
    pub vwc: u8,
    /// Atomic Write Unit Normal
    pub awun: u16,
    /// Atomic Write Unit Power Fail
    pub awupf: u16,
    /// NVM Vendor Specific Command Configuration
    pub nvscc: u8,
    /// Reserved
    pub _resv4: [u8; 173],
    /// Reserved (I/O Command Set Attributes)
    pub _resv5: [u8; 1344],

    // bytes 2048-3071 - Power State Descriptors
    /// Power State Descriptors (PSD0-PSD31)
    pub psd: [PowerStateDescriptor; 32],

    // bytes 3072-4095 - Vendor Specific
    /// Vendor Specific
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
            sqes: 0,
            cqes: 0,
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
            _resv2: [0; 246],
            _resv3: [0; 2],
            _resv4: [0; 173],
            _resv5: [0; 1344],
        }
    }
}

#[derive(Default, Copy, Clone)]
#[repr(C)]
pub struct LbaFormat {
    /// Metadata Size
    pub ms: u16,
    /// LBA Data Size
    pub lbads: u8,
    /// Relative Performance
    pub rp: u8,
}

#[derive(Copy, Clone)]
#[repr(C)]
pub struct IdentifyNamespace {
    /// Namespace Size
    pub nsze: u64,
    /// Namespace Capacity
    pub ncap: u64,
    /// Namespace Utilization
    pub nuse: u64,
    /// Namespace Features
    pub nsfeat: u8,
    /// Number of LBA Formats
    pub nlbaf: u8,
    /// Formatted LBA Size
    pub flbas: u8,
    /// Metadata Capabilities
    pub mc: u8,
    /// End-to-end Data Protection Capabilities
    pub dpc: u8,
    /// End-to-end Data Protection Type Settings
    pub dps: u8,
    /// Reserved
    pub _resv1: [u8; 98],
    /// LBA Formats
    pub lbaf: [LbaFormat; 16],
    /// Reserved
    pub _resv2: [u8; 192],
    /// Vendor Specific
    pub vs: [u8; 3712],
}

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
        assert_eq!(size_of::<RawSubmission>(), 64);
        assert_eq!(size_of::<RawCompletion>(), 16);
        assert_eq!(size_of::<PowerStateDescriptor>(), 32);
        assert_eq!(size_of::<IdentifyController>(), 4096);
        assert_eq!(size_of::<LbaFormat>(), 4);
        assert_eq!(size_of::<IdentifyNamespace>(), 4096);
    }
}
