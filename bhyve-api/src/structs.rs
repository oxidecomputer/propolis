use libc::size_t;
use std::os::raw::{c_int, c_uint, c_void};

// 3:0 - segment type
/// Descriptor type flag (0 = system, 1 = code/data)
pub const SEG_ACCESS_S: u32 = 1 << 4;
// 6:5 - DPL
/// Segment present
pub const SEG_ACCESS_P: u32 = 1 << 7;
// 11:8 reserved
/// Available for use by system software
pub const SEG_ACCESS_AVAIL: u32 = 1 << 12;
pub const SEG_ACCESS_L: u32 = 1 << 13;
pub const SEG_ACCESS_DB: u32 = 1 << 14;
pub const SEG_ACCESS_G: u32 = 1 << 15;
pub const SEG_ACCESS_UNUSABLE: u32 = 1 << 16;

#[repr(C)]
#[derive(Copy, Clone)]
pub struct seg_desc {
    pub base: u64,
    pub limit: u32,
    pub access: u32,
}

pub const INOUT_IN: u8 = 1 << 0;

#[repr(C)]
#[derive(Copy, Clone)]
pub struct vm_inout {
    pub eax: u32,
    pub port: u16,
    pub bytes: u8,
    pub flags: u8,

    // fields used only by in-kernel emulation
    addrsize: u8,
    segment: u8,
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct vm_mmio {
    pub bytes: u8,
    pub read: u8,
    pub _pad: [u16; 3],
    pub gpa: u64,
    pub data: u64,
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct vm_rwmsr {
    pub code: u32,
    pub wval: u64,
}

#[repr(C)]
#[derive(Copy, Clone, Default)]
pub struct vm_exit {
    pub exitcode: c_int,
    pub inst_length: c_int,
    pub rip: u64,
    pub u: vm_exit_payload,
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct vm_entry {
    pub cpuid: c_int,
    pub cmd: c_uint,
    pub exit_data: *mut c_void,
    pub u: vm_entry_payload,
}

#[repr(C)]
#[derive(Copy, Clone)]
pub union vm_exit_payload {
    pub inout: vm_inout,
    pub mmio: vm_mmio,
    pub msr: vm_rwmsr,
    pub vmx: vm_exit_vmx,
    pub svm: vm_exit_svm,
    // sized to zero entire union
    empty: [u64; 6],
}

impl Default for vm_exit_payload {
    fn default() -> Self {
        Self { empty: [0u64; 6] }
    }
}

#[repr(C)]
#[derive(Copy, Clone)]
pub union vm_entry_payload {
    pub inout: vm_inout,
    pub mmio: vm_mmio,
    // sized to zero entire union
    empty: [u64; 3],
}

impl Default for vm_entry_payload {
    fn default() -> Self {
        Self { empty: [0u64; 3] }
    }
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct vm_exit_vmx {
    pub status: c_int,
    pub exit_reason: u32,
    pub exit_qualification: u64,
    pub inst_type: c_int,
    pub inst_error: c_int,
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct vm_exit_svm {
    pub exitcode: u64,
    pub exitinfo1: u64,
    pub exitinfo2: u64,
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct vm_exit_msr {
    pub code: u32,
    pub wval: u64,
}

pub const PROT_READ: u8 = 0x1;
pub const PROT_WRITE: u8 = 0x2;
pub const PROT_EXEC: u8 = 0x4;
pub const PROT_ALL: u8 = PROT_READ | PROT_WRITE | PROT_EXEC;

#[repr(C)]
#[derive(Copy, Clone)]
pub struct vm_memmap {
    pub gpa: u64,
    pub segid: c_int,
    pub segoff: i64,
    pub len: size_t,
    pub prot: c_int,
    pub flags: c_int,
}

pub const VM_MEMMAP_F_WIRED: c_int = 0x01;
#[allow(unused)]
pub const VM_MEMMAP_F_IOMMU: c_int = 0x02;

const SPECNAMELEN: usize = 255; // max length of devicename
pub const SEG_NAME_LEN: usize = SPECNAMELEN + 1;

#[repr(C)]
#[derive(Copy, Clone)]
pub struct vm_memseg {
    pub segid: c_int,
    pub len: size_t,
    pub name: [u8; SEG_NAME_LEN],
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct vm_devmem_offset {
    pub segid: c_int,
    pub offset: i64,
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct vm_register {
    pub cpuid: c_int,
    pub regnum: c_int,
    pub regval: u64,
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct vm_seg_desc {
    pub cpuid: c_int,
    pub regnum: c_int,
    pub desc: seg_desc,
}

#[repr(C)]
#[derive(Copy, Clone, Default)]
pub struct vm_intinfo {
    pub vcpuid: c_int,
    pub info1: u64,
    pub info2: u64,
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct vm_exception {
    pub cpuid: c_int,
    pub vector: c_int,
    pub error_code: c_uint,
    pub error_code_valid: c_int,
    pub restart_instruction: c_int,
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct vm_lapic_msi {
    pub msg: u64,
    pub addr: u64,
}

#[repr(C)]
#[derive(Copy, Clone, Default)]
pub struct vm_lapic_irq {
    pub cpuid: c_int,
    pub vector: c_int,
}

#[repr(C)]
#[derive(Copy, Clone, Default)]
pub struct vm_ioapic_irq {
    pub irq: c_int,
}

#[repr(C)]
#[derive(Copy, Clone, Default)]
pub struct vm_isa_irq {
    pub atpic_irq: c_int,
    pub ioapic_irq: c_int,
}

#[repr(C)]
#[derive(Copy, Clone, Default)]
pub struct vm_isa_irq_trigger {
    pub atpic_irq: c_int,
    /// Trigger mode: 0 - Edge triggered, Non-0 - Level triggered
    pub trigger: c_int,
}

#[repr(C)]
#[derive(Copy, Clone, Default)]
pub struct vm_rtc_data {
    pub offset: i32,
    pub value: u8,
}
