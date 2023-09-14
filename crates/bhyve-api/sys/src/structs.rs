// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::io::{Error, ErrorKind, Result};
use std::os::raw::{c_int, c_uint, c_void};

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
#[derive(Copy, Clone, Default)]
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

#[repr(C)]
#[derive(Copy, Clone)]
pub struct vm_memseg {
    pub segid: c_int,
    pub len: size_t,
    pub name: [u8; VM_MAX_SEG_NAMELEN],
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
pub struct vm_nmi {
    pub cpuid: c_int,
}

#[repr(C)]
#[derive(Copy, Clone, Default)]
pub struct vm_suspend {
    /// Acceptable values defined by `vm_suspend_how`
    pub how: u32,
}

// bit definitions for `vm_reinit.flags`
pub const VM_REINIT_F_FORCE_SUSPEND: u64 = 1 << 0;

#[repr(C)]
#[derive(Copy, Clone, Default)]
pub struct vm_reinit {
    pub flags: u64,
}

#[repr(C)]
#[derive(Copy, Clone, Default)]
pub struct vm_vcpu_reset {
    pub vcpuid: c_int,
    // kind values defined in vcpu_reset_kind
    pub kind: u32,
}

// bit definitions for vm_run_state`state
pub const VRS_HALT: u32 = 0;
pub const VRS_INIT: u32 = 1 << 0;
pub const VRS_RUN: u32 = 1 << 1;
pub const VRS_PEND_INIT: u32 = 1 << 14;
pub const VRS_PEND_SIPI: u32 = 1 << 15;

#[repr(C)]
#[derive(Copy, Clone, Default)]
pub struct vm_run_state {
    pub vcpuid: c_int,
    pub state: u32,
    pub sipi_vector: u8,
    pub _pad: [u8; 3],
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct vm_fpu_state {
    pub vcpuid: c_int,
    pub buf: *mut c_void,
    pub len: u64,
}

#[repr(C)]
#[derive(Copy, Clone, Default)]
pub struct vm_fpu_desc_entry {
    pub vfde_feature: u64,
    pub vfde_size: u32,
    pub vfde_off: u32,
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct vm_fpu_desc {
    pub vfd_entry_data: *mut vm_fpu_desc_entry,
    pub vfd_req_size: u64,
    pub vfd_num_entries: u32,
}
impl Default for vm_fpu_desc {
    fn default() -> Self {
        Self {
            vfd_entry_data: std::ptr::null_mut(),
            vfd_req_size: 0,
            vfd_num_entries: 0,
        }
    }
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct vmm_dirty_tracker {
    pub vdt_start_gpa: u64,
    pub vdt_len: size_t,
    pub vdt_pfns: *mut c_void,
}

// Definitions for vm_data_xfer.vdx_flags
pub const VDX_FLAG_READ_COPYIN: u32 = 1 << 0;
pub const VDX_FLAG_WRITE_COPYOUT: u32 = 1 << 1;

// Current max size for vdx_data
pub const VM_DATA_XFER_LIMIT: u32 = 8192;

#[repr(C)]
#[derive(Copy, Clone)]
pub struct vm_data_xfer {
    pub vdx_vcpuid: c_int,
    pub vdx_class: u16,
    pub vdx_version: u16,
    pub vdx_flags: u32,
    pub vdx_len: u32,
    pub vdx_result_len: u32,
    pub vdx_data: *mut c_void,
}
impl Default for vm_data_xfer {
    fn default() -> Self {
        vm_data_xfer {
            vdx_vcpuid: -1,
            vdx_class: 0,
            vdx_version: 0,
            vdx_flags: 0,
            vdx_len: 0,
            vdx_result_len: 0,
            vdx_data: std::ptr::null_mut(),
        }
    }
}

/// Use index (ecx) input value when matching entry
pub const VCE_FLAG_MATCH_INDEX: u32 = 1 << 0;

#[repr(C)]
#[derive(Copy, Clone, Default)]
pub struct vcpu_cpuid_entry {
    pub vce_function: u32,
    pub vce_index: u32,
    pub vce_flags: u32,
    pub vce_eax: u32,
    pub vce_ebx: u32,
    pub vce_ecx: u32,
    pub vce_edx: u32,
    pub _pad: u32,
}
impl vcpu_cpuid_entry {
    fn match_idx(&self) -> bool {
        self.vce_flags & VCE_FLAG_MATCH_INDEX != 0
    }
    /// Order entries for proper cpuid evaluation by the kernel VMM.
    ///
    /// Bhyve expects that cpuid entries are sorted by function, and then index,
    /// from least to greatest.  Entries which must match on index should come
    /// before (less-than) those that do not, so the former can take precedence
    /// in matching.
    ///
    /// This function is provided so that a list of entries can be easily sorted
    /// prior to loading them into the kernel VMM.
    ///
    /// ```
    /// let mut entries: Vec<vcpu_cpuid_entry> = vec![
    ///     // entries loaded here
    /// ];
    /// entries.sort_by(vcpu_cpuid_entry::eval_sort);
    /// let config = vm_vcpu_cpuid_config {
    ///     vvcc_cpuid: 0,
    ///     vvcc_flags: 0,
    ///     vvcc_nent: entries.len(),
    ///     vvcc_entries: &mut entries,
    /// };
    /// // perform ioctl(VM_SET_CPUID, &config) ...
    /// ```
    pub fn eval_sort(a: &Self, b: &Self) -> std::cmp::Ordering {
        use std::cmp::Ordering;

        match a.vce_function.cmp(&b.vce_function) {
            Ordering::Equal => match (a.match_idx(), b.match_idx()) {
                (true, false) => Ordering::Less,
                (false, true) => Ordering::Greater,
                (true, true) | (false, false) => a.vce_index.cmp(&b.vce_index),
            },

            ord => ord,
        }
    }
}

/// Use legacy hard-coded cpuid masking tables applied to the host CPU
pub const VCC_FLAG_LEGACY_HANDLING: u32 = 1 << 0;

/// Emulate Intel-style fallback behavior (emit highest "standard" entry) if the
/// queried function/index do not match.  If not set, emulate AMD-style, where
/// all zeroes are returned in such cases.
pub const VCC_FLAG_INTEL_FALLBACK: u32 = 1 << 1;

#[repr(C)]
#[derive(Copy, Clone)]
pub struct vm_vcpu_cpuid_config {
    pub vvcc_vcpuid: c_int,
    pub vvcc_flags: u32,
    pub vvcc_nent: u32,
    pub _pad: u32,
    pub vvcc_entries: *mut c_void,
}
impl Default for vm_vcpu_cpuid_config {
    fn default() -> Self {
        Self {
            vvcc_vcpuid: 0,
            vvcc_flags: 0,
            vvcc_nent: 0,
            _pad: 0,
            vvcc_entries: std::ptr::null_mut(),
        }
    }
}

#[repr(C)]
#[derive(Copy, Clone, Default)]
pub struct vm_legacy_cpuid {
    pub vlc_vcpuid: c_int,
    pub vlc_eax: u32,
    pub vlc_ebx: u32,
    pub vlc_ecx: u32,
    pub vlc_edx: u32,
}

pub const VM_MAX_NAMELEN: usize = 128;
pub const VM_MAX_SEG_NAMELEN: usize = 128;

/// Copy VM name into array appropriately sized for create/destroy request.
/// Advanced checks are left to the kernel logic consuming that value.
fn validate_name(value: &[u8]) -> Result<[u8; VM_MAX_NAMELEN]> {
    let mut buf = [0u8; VM_MAX_NAMELEN];

    if value.len() > buf.len() {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            "name length exceeds VM_MAX_NAMELEN",
        ));
    }

    buf[..(value.len())].copy_from_slice(value);
    Ok(buf)
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
    pub fn new(name: &[u8]) -> Result<Self> {
        Ok(Self { name: validate_name(name)?, flags: 0 })
    }
}

// Flag values for use in in `vm_create_req`:

// Allocate guest memory segments from existing reservoir capacity, rather than
// attempting to create transient allocations.
pub const VCF_RESERVOIR_MEM: u64 = 1;

/// Enable dirty page tracking for the guest.
pub const VCF_TRACK_DIRTY: u64 = 1 << 1;

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
    pub fn new(name: &[u8]) -> Result<Self> {
        Ok(Self { name: validate_name(name)? })
    }
}

#[repr(C)]
#[derive(Copy, Clone, Default)]
pub struct vmm_resv_query {
    pub vrq_free_sz: size_t,
    pub vrq_alloc_sz: size_t,
    pub vrq_alloc_transient_sz: size_t,
    pub vrq_limit: size_t,
}

#[repr(C)]
#[derive(Copy, Clone, Default)]
pub struct vmm_resv_target {
    /// Target size for VMM reservoir
    pub vrt_target_sz: size_t,

    /// Change of reservoir size to meet target will be done in multiple steps
    /// of chunk size (or smaller)
    pub vrt_chunk_sz: size_t,

    /// Resultant size of reservoir after operation.  Should match target size,
    /// except when interrupted.
    pub vrt_result_sz: size_t,
}
