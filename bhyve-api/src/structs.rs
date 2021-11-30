use std::os::raw::{c_int, c_uint, c_void};

use bitflags::bitflags;
use libc::size_t;

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
    pub inst_emul: vm_inst_emul,
    pub suspend: c_int,
    pub paging: vm_paging,
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

#[repr(C)]
#[derive(Copy, Clone)]
pub struct vm_inst_emul {
    pub inst: [u8; 15],
    pub num_valid: u8,
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct vm_paging {
    pub gpa: u64,
    pub fault_type: c_int,
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

#[repr(C)]
#[derive(Copy, Clone, Default)]
pub struct vm_capability {
    pub cpuid: c_int,
    pub captype: c_int,
    pub capval: c_int,
    pub allcpus: c_int,
}

#[repr(C)]
#[derive(Copy, Clone, Default)]
pub struct vm_suspend {
    /// Acceptable values defined by `vm_suspend_how`
    pub how: u32,
}

// bit definitions for `vm_reinit.flags`
bitflags! {
    #[repr(C)]
    #[derive(Default)]
    pub struct VmReinitFlags: u64 {
        const FORCE_SUSPEND = (1 << 0);
    }
}

#[repr(C)]
#[derive(Copy, Clone, Default)]
pub struct vm_reinit {
    pub flags: VmReinitFlags,
}

#[repr(C)]
#[derive(Copy, Clone, Default)]
pub struct vm_vcpu_reset {
    pub cpuid: c_int,
    // kind values defined in vcpu_reset_kind
    pub kind: u32,
}

// bit definitions for vm_run_state`state
pub const VRS_HALT: u32 = 0;
pub const VRS_INIT: u32 = 1 << 0;
pub const VRS_RUN: u32 = 1 << 1;
pub const VRS_PEND_SIPI: u32 = 1 << 14;
pub const VRS_PEND_INIT: u32 = 1 << 15;

#[repr(C)]
#[derive(Copy, Clone, Default)]
pub struct vm_run_state {
    pub cpuid: c_int,
    pub state: u32,
    pub sipi_vector: u8,
    pub _pad: [u8; 3],
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct vm_dirty_tracker {
    pub vdt_start_gpa: u64,
    pub vdt_len: size_t,
    pub vdt_pfns: *mut c_void,
}

pub const VM_MAX_NAMELEN: usize = 128;
pub const VM_MAX_SEG_NAMELEN: usize = 128;

// Copy VM name, paying no heed to whether a trailing NUL is left in the
// destination byte slice.  The kernel will do that error handling.
fn copy_name(field: &mut [u8], name: &str) {
    let copy_len = name.len().min(field.len());
    field[..copy_len].copy_from_slice(name.as_bytes());
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct vm_create_req {
    pub name: [u8; VM_MAX_NAMELEN],
    pub flags: u64,
}
impl Default for vm_create_req {
    fn default() -> Self {
        Self { name: [0u8; VM_MAX_NAMELEN], flags: 0 }
    }
}
impl vm_create_req {
    pub fn new(name: &str) -> Self {
        let mut res = Self::default();
        copy_name(&mut res.name, name);
        res
    }
}

// Flag values for use in in `vm_create_req`:

// Allocate guest memory segments from existing reservoir capacity, rather than
// attempting to create transient allocations.
pub const VCF_RESERVOIR_MEM: u64 = 1;

#[repr(C)]
#[derive(Copy, Clone)]
pub struct vm_destroy_req {
    pub name: [u8; VM_MAX_NAMELEN],
}
impl Default for vm_destroy_req {
    fn default() -> Self {
        Self { name: [0u8; VM_MAX_NAMELEN] }
    }
}
impl vm_destroy_req {
    pub fn new(name: &str) -> Self {
        let mut res = Self::default();
        copy_name(&mut res.name, name);
        res
    }
}
