use num_enum::TryFromPrimitive;

#[repr(C)]
#[allow(non_camel_case_types, unused)]
#[derive(Copy, Clone, Debug)]
pub enum vm_reg_name {
    VM_REG_GUEST_RAX,
    VM_REG_GUEST_RBX,
    VM_REG_GUEST_RCX,
    VM_REG_GUEST_RDX,
    VM_REG_GUEST_RSI,
    VM_REG_GUEST_RDI,
    VM_REG_GUEST_RBP,
    VM_REG_GUEST_R8,
    VM_REG_GUEST_R9,
    VM_REG_GUEST_R10,
    VM_REG_GUEST_R11,
    VM_REG_GUEST_R12,
    VM_REG_GUEST_R13,
    VM_REG_GUEST_R14,
    VM_REG_GUEST_R15,
    VM_REG_GUEST_CR0,
    VM_REG_GUEST_CR3,
    VM_REG_GUEST_CR4,
    VM_REG_GUEST_DR7,
    VM_REG_GUEST_RSP,
    VM_REG_GUEST_RIP,
    VM_REG_GUEST_RFLAGS,
    VM_REG_GUEST_ES,
    VM_REG_GUEST_CS,
    VM_REG_GUEST_SS,
    VM_REG_GUEST_DS,
    VM_REG_GUEST_FS,
    VM_REG_GUEST_GS,
    VM_REG_GUEST_LDTR,
    VM_REG_GUEST_TR,
    VM_REG_GUEST_IDTR,
    VM_REG_GUEST_GDTR,
    VM_REG_GUEST_EFER,
    VM_REG_GUEST_CR2,
    VM_REG_GUEST_PDPTE0,
    VM_REG_GUEST_PDPTE1,
    VM_REG_GUEST_PDPTE2,
    VM_REG_GUEST_PDPTE3,
    VM_REG_GUEST_INTR_SHADOW,
    VM_REG_GUEST_DR0,
    VM_REG_GUEST_DR1,
    VM_REG_GUEST_DR2,
    VM_REG_GUEST_DR3,
    VM_REG_GUEST_DR6,
    VM_REG_GUEST_ENTRY_INST_LENGTH,
    VM_REG_LAST,
}

#[repr(i32)]
#[allow(non_camel_case_types, unused)]
#[derive(TryFromPrimitive)]
pub enum vm_exitcode {
    VM_EXITCODE_INOUT,
    VM_EXITCODE_VMX,
    VM_EXITCODE_BOGUS,
    VM_EXITCODE_RDMSR,
    VM_EXITCODE_WRMSR,
    VM_EXITCODE_HLT,
    VM_EXITCODE_MTRAP,
    VM_EXITCODE_PAUSE,
    VM_EXITCODE_PAGING,
    VM_EXITCODE_INST_EMUL,
    VM_EXITCODE_RUN_STATE,
    VM_EXITCODE_MMIO_EMUL,
    VM_EXITCODE_DEPRECATED,
    VM_EXITCODE_IOAPIC_EOI,
    VM_EXITCODE_SUSPENDED,
    VM_EXITCODE_MMIO,
    VM_EXITCODE_TASK_SWITCH,
    VM_EXITCODE_MONITOR,
    VM_EXITCODE_MWAIT,
    VM_EXITCODE_SVM,
    VM_EXITCODE_REQIDLE,
    VM_EXITCODE_DEBUG,
    VM_EXITCODE_VMINSN,
    VM_EXITCODE_BPT,
    VM_EXITCODE_HT,
}

#[repr(u32)]
#[allow(non_camel_case_types, unused)]
pub enum vcpu_reset_kind {
    VRK_RESET = 0,
    VRK_INIT = 1,
}

#[repr(u32)]
#[allow(non_camel_case_types, unused)]
pub enum vm_entry_cmds {
    VEC_DEFAULT = 0,
    VEC_DISCARD_INSTR,
    VEC_FULFILL_MMIO,
    VEC_FULFILL_INOUT,
}

#[repr(i32)]
#[allow(non_camel_case_types, unused)]
pub enum vm_cap_type {
    VM_CAP_HALT_EXIT,
    VM_CAP_MTRAP_EXIT,
    VM_CAP_PAUSE_EXIT,
    VM_CAP_ENABLE_INVPCID,
    VM_CAP_BPT_EXIT,
}
