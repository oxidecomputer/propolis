// Define constants from machine/vmm_dev.h

const VMMCTL_IOC_BASE: i32 = ((b'V' as i32) << 16) | ((b'M' as i32) << 8);
const VMM_IOC_BASE: i32 = ((b'v' as i32) << 16) | ((b'm' as i32) << 8);
const VMM_LOCK_IOC_BASE: i32 = ((b'v' as i32) << 16) | ((b'l' as i32) << 8);
const VMM_CPU_IOC_BASE: i32 = ((b'v' as i32) << 16) | ((b'p' as i32) << 8);

// Operations performed on the vmmctl device
pub const VMM_CREATE_VM: i32 = VMMCTL_IOC_BASE | 0x01;
pub const VMM_DESTROY_VM: i32 = VMMCTL_IOC_BASE | 0x02;
pub const VMM_VM_SUPPORTED: i32 = VMMCTL_IOC_BASE | 0x03;

// Operations performed in the context of a given vCPU
pub const VM_RUN: i32 = VMM_CPU_IOC_BASE | 0x01;
pub const VM_SET_REGISTER: i32 = VMM_CPU_IOC_BASE | 0x02;
pub const VM_GET_REGISTER: i32 = VMM_CPU_IOC_BASE | 0x03;
pub const VM_SET_SEGMENT_DESCRIPTOR: i32 = VMM_CPU_IOC_BASE | 0x04;
pub const VM_GET_SEGMENT_DESCRIPTOR: i32 = VMM_CPU_IOC_BASE | 0x05;
pub const VM_SET_REGISTER_SET: i32 = VMM_CPU_IOC_BASE | 0x06;
pub const VM_GET_REGISTER_SET: i32 = VMM_CPU_IOC_BASE | 0x07;
pub const VM_INJECT_EXCEPTION: i32 = VMM_CPU_IOC_BASE | 0x08;
pub const VM_SET_CAPABILITY: i32 = VMM_CPU_IOC_BASE | 0x09;
pub const VM_GET_CAPABILITY: i32 = VMM_CPU_IOC_BASE | 0x0a;
pub const VM_PPTDEV_MSI: i32 = VMM_CPU_IOC_BASE | 0x0b;
pub const VM_PPTDEV_MSIX: i32 = VMM_CPU_IOC_BASE | 0x0c;
pub const VM_SET_X2APIC_STATE: i32 = VMM_CPU_IOC_BASE | 0x0d;
pub const VM_GLA2GPA: i32 = VMM_CPU_IOC_BASE | 0x0e;
pub const VM_GLA2GPA_NOFAULT: i32 = VMM_CPU_IOC_BASE | 0x0f;
pub const VM_ACTIVATE_CPU: i32 = VMM_CPU_IOC_BASE | 0x10;
pub const VM_SET_INTINFO: i32 = VMM_CPU_IOC_BASE | 0x11;
pub const VM_GET_INTINFO: i32 = VMM_CPU_IOC_BASE | 0x12;
pub const VM_RESTART_INSTRUCTION: i32 = VMM_CPU_IOC_BASE | 0x13;
pub const VM_SET_KERNEMU_DEV: i32 = VMM_CPU_IOC_BASE | 0x14;
pub const VM_GET_KERNEMU_DEV: i32 = VMM_CPU_IOC_BASE | 0x15;
pub const VM_RESET_CPU: i32 = VMM_CPU_IOC_BASE | 0x16;
pub const VM_GET_RUN_STATE: i32 = VMM_CPU_IOC_BASE | 0x17;
pub const VM_SET_RUN_STATE: i32 = VMM_CPU_IOC_BASE | 0x18;

// Operations requiring write-locking the VM
pub const VM_REINIT: i32 = VMM_LOCK_IOC_BASE | 0x01;
pub const VM_BIND_PPTDEV: i32 = VMM_LOCK_IOC_BASE | 0x02;
pub const VM_UNBIND_PPTDEV: i32 = VMM_LOCK_IOC_BASE | 0x03;
pub const VM_MAP_PPTDEV_MMIO: i32 = VMM_LOCK_IOC_BASE | 0x04;
pub const VM_ALLOC_MEMSEG: i32 = VMM_LOCK_IOC_BASE | 0x05;
pub const VM_MMAP_MEMSEG: i32 = VMM_LOCK_IOC_BASE | 0x06;
pub const VM_PMTMR_LOCATE: i32 = VMM_LOCK_IOC_BASE | 0x07;

pub const VM_WRLOCK_CYCLE: i32 = VMM_LOCK_IOC_BASE | 0xff;

// All other ioctls
pub const VM_GET_GPA_PMAP: i32 = VMM_IOC_BASE | 0x01;
pub const VM_GET_MEMSEG: i32 = VMM_IOC_BASE | 0x02;
pub const VM_MMAP_GETNEXT: i32 = VMM_IOC_BASE | 0x03;

pub const VM_LAPIC_IRQ: i32 = VMM_IOC_BASE | 0x04;
pub const VM_LAPIC_LOCAL_IRQ: i32 = VMM_IOC_BASE | 0x05;
pub const VM_LAPIC_MSI: i32 = VMM_IOC_BASE | 0x06;

pub const VM_IOAPIC_ASSERT_IRQ: i32 = VMM_IOC_BASE | 0x07;
pub const VM_IOAPIC_DEASSERT_IRQ: i32 = VMM_IOC_BASE | 0x08;
pub const VM_IOAPIC_PULSE_IRQ: i32 = VMM_IOC_BASE | 0x09;

pub const VM_ISA_ASSERT_IRQ: i32 = VMM_IOC_BASE | 0x0a;
pub const VM_ISA_DEASSERT_IRQ: i32 = VMM_IOC_BASE | 0x0b;
pub const VM_ISA_PULSE_IRQ: i32 = VMM_IOC_BASE | 0x0c;
pub const VM_ISA_SET_IRQ_TRIGGER: i32 = VMM_IOC_BASE | 0x0d;

pub const VM_RTC_WRITE: i32 = VMM_IOC_BASE | 0x0e;
pub const VM_RTC_READ: i32 = VMM_IOC_BASE | 0x0f;
pub const VM_RTC_SETTIME: i32 = VMM_IOC_BASE | 0x10;
pub const VM_RTC_GETTIME: i32 = VMM_IOC_BASE | 0x11;

pub const VM_SUSPEND: i32 = VMM_IOC_BASE | 0x12;

pub const VM_IOAPIC_PINCOUNT: i32 = VMM_IOC_BASE | 0x13;
pub const VM_GET_PPTDEV_LIMITS: i32 = VMM_IOC_BASE | 0x14;
pub const VM_GET_HPET_CAPABILITIES: i32 = VMM_IOC_BASE | 0x15;

pub const VM_STATS_IOC: i32 = VMM_IOC_BASE | 0x16;
pub const VM_STAT_DESC: i32 = VMM_IOC_BASE | 0x17;

pub const VM_INJECT_NMI: i32 = VMM_IOC_BASE | 0x18;
pub const VM_GET_X2APIC_STATE: i32 = VMM_IOC_BASE | 0x19;
pub const VM_SET_TOPOLOGY: i32 = VMM_IOC_BASE | 0x1a;
pub const VM_GET_TOPOLOGY: i32 = VMM_IOC_BASE | 0x1b;
pub const VM_GET_CPUS: i32 = VMM_IOC_BASE | 0x1c;
pub const VM_SUSPEND_CPU: i32 = VMM_IOC_BASE | 0x1d;
pub const VM_RESUME_CPU: i32 = VMM_IOC_BASE | 0x1e;
pub const VM_TRACK_DIRTY_PAGES: i32 = VMM_IOC_BASE | 0x20;

pub const VM_DEVMEM_GETOFFSET: i32 = VMM_IOC_BASE | 0xff;
