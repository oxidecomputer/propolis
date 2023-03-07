pub mod ioctls {
    const VNA_IOC: i32 = ((b'V' as i32) << 16) | ((b'C' as i32) << 8);

    pub const VNA_IOC_CREATE: i32 = VNA_IOC | 0x01;
    pub const VNA_IOC_DELETE: i32 = VNA_IOC | 0x02;
    pub const VNA_IOC_VERSION: i32 = VNA_IOC | 0x03;

    pub const VNA_IOC_RING_INIT: i32 = VNA_IOC | 0x10;
    pub const VNA_IOC_RING_RESET: i32 = VNA_IOC | 0x11;
    pub const VNA_IOC_RING_KICK: i32 = VNA_IOC | 0x12;
    pub const VNA_IOC_RING_SET_MSI: i32 = VNA_IOC | 0x13;
    pub const VNA_IOC_RING_INTR_CLR: i32 = VNA_IOC | 0x14;
    pub const VNA_IOC_RING_SET_STATE: i32 = VNA_IOC | 0x15;
    pub const VNA_IOC_RING_GET_STATE: i32 = VNA_IOC | 0x16;
    pub const VNA_IOC_RING_PAUSE: i32 = VNA_IOC | 0x17;

    pub const VNA_IOC_INTR_POLL: i32 = VNA_IOC | 0x20;
    pub const VNA_IOC_SET_FEATURES: i32 = VNA_IOC | 0x21;
    pub const VNA_IOC_GET_FEATURES: i32 = VNA_IOC | 0x22;
    pub const VNA_IOC_SET_NOTIFY_IOP: i32 = VNA_IOC | 0x23;
}

pub const VIONA_VQ_MAX: u16 = 2;

mod structs {
    #![allow(non_camel_case_types)]

    use super::VIONA_VQ_MAX;

    #[repr(C)]
    pub struct vioc_create {
        pub c_linkid: u32,
        pub c_vmfd: i32,
    }

    #[repr(C)]
    pub struct vioc_ring_init {
        pub ri_index: u16,
        pub ri_qsize: u16,
        pub _pad: [u16; 2],
        pub ri_qaddr: u64,
    }

    #[repr(C)]
    pub struct vioc_ring_msi {
        pub rm_index: u16,
        pub _pad: [u16; 3],
        pub rm_addr: u64,
        pub rm_msg: u64,
    }

    #[repr(C)]
    pub struct vioc_intr_poll {
        pub vip_status: [u32; VIONA_VQ_MAX as usize],
    }

    #[repr(C)]
    #[derive(Default)]
    pub struct vioc_ring_state {
        pub vrs_index: u16,
        pub vrs_avail_idx: u16,
        pub vrs_used_idx: u16,
        pub vrs_qsize: u16,
        pub vrs_qaddr: u64,
    }
}

/// This is the viona interface version which viona_api expects to operate
/// against.  All constants and structs defined by the crate are done so in
/// terms of that specific version.
pub const VIONA_CURRENT_INTERFACE_VERSION: u32 = 2;

pub use ioctls::*;
pub use structs::*;
