#![allow(non_camel_case_types)]
// VMM Data Classes

pub const VDC_VERSION: u16 = 1;

// Classes bearing per-vCPU data

pub const VDC_REGISTER: u16 = 2;
pub const VDC_MSR: u16 = 3;
pub const VDC_FPU: u16 = 4;
pub const VDC_LAPIC: u16 = 5;
pub const VDC_VMM_ARCH: u16 = 6;

// Classes for system-wide device state

pub const VDC_IOAPIC: u16 = 7;
pub const VDC_ATPIT: u16 = 8;
pub const VDC_ATPIC: u16 = 9;
pub const VDC_HPET: u16 = 10;
pub const VDC_PM_TIMER: u16 = 11;
pub const VDC_RTC: u16 = 12;
pub const VDC_VMM_TIME: u16 = 13;

#[repr(C)]
#[derive(Copy, Clone, Default)]
pub struct vdi_version_entry_v1 {
    pub vve_class: u16,
    pub vve_version: u16,
    pub vve_len_expect: u16,
    pub vve_len_per_item: u16,
}

#[repr(C)]
#[derive(Copy, Clone, Default)]
pub struct vdi_field_entry_v1 {
    pub vfe_ident: u32,
    pub _pad: u32,
    pub vfe_value: u64,
}

#[repr(C)]
#[derive(Copy, Clone, Default)]
pub struct vdi_lapic_page_v1 {
    pub vlp_id: u32,
    pub vlp_version: u32,
    pub vlp_tpr: u32,
    pub vlp_apr: u32,
    pub vlp_ldr: u32,
    pub vlp_dfr: u32,
    pub vlp_svr: u32,
    pub vlp_isr: [u32; 8],
    pub vlp_tmr: [u32; 8],
    pub vlp_irr: [u32; 8],
    pub vlp_esr: u32,
    pub vlp_lvt_cmci: u32,
    pub vlp_icr: u64,
    pub vlp_lvt_timer: u32,
    pub vlp_lvt_thermal: u32,
    pub vlp_lvt_pcint: u32,
    pub vlp_lvt_lint0: u32,
    pub vlp_lvt_lint1: u32,
    pub vlp_lvt_error: u32,
    pub vlp_icr_timer: u32,
    pub vlp_dcr_timer: u32,
}

#[repr(C)]
#[derive(Copy, Clone, Default)]
pub struct vdi_lapic_v1 {
    pub vl_lapic: vdi_lapic_page_v1,
    pub vl_msr_apicbase: u64,
    pub vl_timer_target: i64,
    pub vl_esr_pending: u32,
}

#[repr(C)]
#[derive(Copy, Clone, Default)]
pub struct vdi_ioapic_v1 {
    pub vi_pin_reg: [u64; 32],
    pub vi_pin_level: [u32; 32],
    pub vi_id: u32,
    pub vi_reg_sel: u32,
}

#[repr(C)]
#[derive(Copy, Clone, Default)]
pub struct vdi_atpit_channel_v1 {
    pub vac_initial: u16,
    pub vac_reg_cr: u16,
    pub vac_reg_ol: u16,
    pub vac_reg_status: u8,
    pub vac_mode: u8,

    /// `vac_status` bits:
    /// - 0b00001 status latched
    /// - 0b00010 output latched
    /// - 0b00100 control register sel
    /// - 0b01000 output latch sel
    /// - 0b10000 free-running timer
    pub vac_status: u8,

    pub vac_time_target: i64,
}

#[repr(C)]
#[derive(Copy, Clone, Default)]
pub struct vdi_atpit_v1 {
    pub va_channel: [vdi_atpit_channel_v1; 3],
}

#[repr(C)]
#[derive(Copy, Clone, Default)]
pub struct vdi_atpic_chip_v1 {
    pub vac_icw_state: u8,

    /// vac_status bits:
    /// - 0b00000001 ready
    /// - 0b00000010 auto EOI
    /// - 0b00000100 poll
    /// - 0b00001000 rotate
    /// - 0b00010000 special full nested
    /// - 0b00100000 read isr next
    /// - 0b01000000 intr raised
    /// - 0b10000000 special mask mode
    pub vac_status: u8,
    pub vac_reg_irr: u8,
    pub vac_reg_isr: u8,
    pub vac_reg_imr: u8,
    pub vac_irq_base: u8,
    pub vac_lowprio: u8,
    pub vac_elc: u8,
    pub vac_level: [u32; 8],
}

#[repr(C)]
#[derive(Copy, Clone, Default)]
pub struct vdi_atpic_v1 {
    pub va_chip: [vdi_atpic_chip_v1; 2],
}

#[repr(C)]
#[derive(Copy, Clone, Default)]
pub struct vdi_hpet_timer_v1 {
    pub vht_config: u64,
    pub vht_msi: u64,
    pub vht_comp_val: u32,
    pub vht_comp_rate: u32,
    pub vht_time_target: i64,
}

#[repr(C)]
#[derive(Copy, Clone, Default)]
pub struct vdi_hpet_v1 {
    pub vh_config: u64,
    pub vh_isr: u64,
    pub vh_count_base: u32,
    pub vh_time_base: i64,

    pub vh_timers: [vdi_hpet_timer_v1; 8],
}

#[repr(C)]
#[derive(Copy, Clone, Default)]
pub struct vdi_pm_timer_v1 {
    pub vpt_time_base: i64,
    pub vpt_ioport: u16,
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct vdi_rtc_v1 {
    pub vr_content: [u8; 128],
    pub vr_addr: u8,
    pub vr_time_base: i64,
    pub vr_rtc_sec: u64,
    pub vr_rtc_nsec: u64,
}
impl Default for vdi_rtc_v1 {
    fn default() -> Self {
        vdi_rtc_v1 {
            vr_content: [0u8; 128],
            vr_addr: 0,
            vr_time_base: 0,
            vr_rtc_sec: 0,
            vr_rtc_nsec: 0,
        }
    }
}

#[repr(C)]
#[derive(Copy, Clone, Serialize, Deserialize)]
pub struct vdi_rtc_v2 {
    pub vr_base_clock: i64,
    pub vr_last_period: i64,
    #[serde(with = "serde_arrays")]
    pub vr_content: [u8; 128],
    pub vr_addr: u8,
}
impl Default for vdi_rtc_v2 {
    fn default() -> Self {
        vdi_rtc_v2 {
            vr_base_clock: 0,
            vr_last_period: 0,
            vr_content: [0u8; 128],
            vr_addr: 0,
        }
    }
}

// VDC_VMM_TIME v1 interface

#[repr(C)]
#[derive(Copy, Clone, Default)]
pub struct vdi_time_info_v1 {
    pub vt_guest_freq: u64,
    pub vt_guest_tsc: u64,
    pub vt_boot_hrtime: i64,
    pub vt_hrtime: i64,
    pub vt_hres_sec: u64,
    pub vt_hres_ns: u64,
}

// VDC_VMM_ARCH v1 data identifiers

// VM-wide:

/// Guest instance has been placed in paused state
pub const VAI_VM_IS_PAUSED: u32 = 4;

// Time-related data which was superseded by the VMM_TIME interface in API
// version 11, maintained here for reference (and for older versions)

/// Offset of guest TSC from system at time of boot
pub const VAI_TSC_BOOT_OFFSET: u32 = 1;
/// Time that guest (nominally) booted, as hrtime
pub const VAI_BOOT_HRTIME: u32 = 2;
/// Guest TSC frequency measured by hrtime (not effected by wall clock adj.)
pub const VAI_TSC_FREQ: u32 = 3;

// per-vCPU

/// NMI pending injection for vCPU (0 or 1)
pub const VAI_PEND_NMI: u32 = 10;
/// extint pending injection for vCPU (0 or 1)
pub const VAI_PEND_EXTINT: u32 = 11;
/// HW exception pending injection for vCPU
pub const VAI_PEND_EXCP: u32 = 12;
/// exception/interrupt pending injection for vCPU
pub const VAI_PEND_INTINFO: u32 = 13;
