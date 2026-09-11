// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

#![allow(non_camel_case_types)]

use libc::size_t;
use std::ffi::c_void;

const fn vna_ioc(ioc: i32) -> i32 {
    const V: i32 = b'V' as i32;
    const C: i32 = b'C' as i32;
    V << 16 | C << 8 | ioc
}

pub const VNA_IOC_CREATE: i32 = vna_ioc(0x01);
pub const VNA_IOC_DELETE: i32 = vna_ioc(0x02);
pub const VNA_IOC_VERSION: i32 = vna_ioc(0x03);
pub const VNA_IOC_DEFAULT_PARAMS: i32 = vna_ioc(0x04);

pub const VNA_IOC_RING_INIT: i32 = vna_ioc(0x10);
pub const VNA_IOC_RING_RESET: i32 = vna_ioc(0x11);
pub const VNA_IOC_RING_KICK: i32 = vna_ioc(0x12);
pub const VNA_IOC_RING_SET_MSI: i32 = vna_ioc(0x13);
pub const VNA_IOC_RING_INTR_CLR: i32 = vna_ioc(0x14);
pub const VNA_IOC_RING_SET_STATE: i32 = vna_ioc(0x15);
pub const VNA_IOC_RING_GET_STATE: i32 = vna_ioc(0x16);
pub const VNA_IOC_RING_PAUSE: i32 = vna_ioc(0x17);
pub const VNA_IOC_RING_INIT_MODERN: i32 = vna_ioc(0x18);

pub const VNA_IOC_INTR_POLL: i32 = vna_ioc(0x20);
pub const VNA_IOC_SET_FEATURES: i32 = vna_ioc(0x21);
pub const VNA_IOC_GET_FEATURES: i32 = vna_ioc(0x22);
pub const VNA_IOC_SET_NOTIFY_IOP: i32 = vna_ioc(0x23);
pub const VNA_IOC_SET_PROMISC: i32 = vna_ioc(0x24);
pub const VNA_IOC_GET_PARAMS: i32 = vna_ioc(0x25);
pub const VNA_IOC_SET_PARAMS: i32 = vna_ioc(0x26);
pub const VNA_IOC_GET_MTU: i32 = vna_ioc(0x27);
pub const VNA_IOC_SET_MTU: i32 = vna_ioc(0x28);
pub const VNA_IOC_SET_NOTIFY_MMIO: i32 = vna_ioc(0x29);
pub const VNA_IOC_INTR_POLL_MQ: i32 = vna_ioc(0x2a);

/// VirtIO 1.2 queue pair support.
pub const VNA_IOC_GET_PAIRS: i32 = vna_ioc(0x30);
pub const VNA_IOC_SET_PAIRS: i32 = vna_ioc(0x31);
pub const VNA_IOC_GET_USEPAIRS: i32 = vna_ioc(0x32);
pub const VNA_IOC_SET_USEPAIRS: i32 = vna_ioc(0x33);

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_vna_ioc() {
        assert_eq!(vna_ioc(0x22), 0x00_56_43_22);
    }
}

/// The minimum number of queue pairs supported by a device.
pub const VIONA_MIN_QPAIR: usize = 1;

/// The maximum number of queue pairs supported by a device.
///
/// Note that the VirtIO limit is much higher (0x8000); Viona artificially
/// limits the number to 256 pairs, which makes it possible to implmeent
/// interrupt notification with a reasonably sized bitmap.
pub const VIONA_MAX_QPAIR: usize = 0x100;

const fn howmany(x: usize, y: usize) -> usize {
    assert!(y > 0);
    x.div_ceil(y)
}

/// The number of 32-bit words required to detect interrupts for the maximum
/// number of supported queue pairs.  Note the factor of two here: interrupts
/// are per-queue, not per-pair.
pub const VIONA_INTR_WORDS: usize = howmany(VIONA_MAX_QPAIR * 2, 32);

#[repr(C)]
pub struct vioc_create {
    pub c_linkid: u32,
    pub c_vmfd: i32,
}

#[repr(C)]
#[derive(Default)]
pub struct vioc_ring_init_modern {
    pub rim_index: u16,
    pub rim_qsize: u16,
    pub _pad: [u16; 2],
    pub rim_qaddr_desc: u64,
    pub rim_qaddr_avail: u64,
    pub rim_qaddr_used: u64,
}

#[repr(C)]
#[derive(Default)]
pub struct vioc_ring_msi {
    pub rm_index: u16,
    pub _pad: [u16; 3],
    pub rm_addr: u64,
    pub rm_msg: u64,
}

#[repr(C)]
#[derive(Default)]
pub struct vioc_intr_poll_mq {
    pub vipm_nrings: u16,
    pub _pad: u16,
    pub vipm_status: [u32; VIONA_INTR_WORDS],
}

#[repr(C)]
#[derive(Default)]
pub struct vioc_notify_mmio {
    pub vim_address: u64,
    pub vim_size: u32,
}

#[repr(C)]
#[derive(Default)]
pub struct vioc_ring_state {
    pub vrs_index: u16,
    pub vrs_avail_idx: u16,
    pub vrs_used_idx: u16,
    pub vrs_qsize: u16,
    pub vrs_qaddr_desc: u64,
    pub vrs_qaddr_avail: u64,
    pub vrs_qaddr_used: u64,
}

pub const VIONA_PROMISC_NONE: i32 = 0;
pub const VIONA_PROMISC_MULTI: i32 = 1;
pub const VIONA_PROMISC_ALL: i32 = 2;
#[cfg(feature = "falcon")]
pub const VIONA_PROMISC_ALL_VLAN: i32 = 3;

#[repr(C)]
#[derive(Default)]
pub struct vioc_get_params {
    pub vgp_param: *mut c_void,
    pub vgp_param_sz: size_t,
}

#[repr(C)]
#[derive(Default)]
pub struct vioc_set_params {
    pub vsp_param: *mut c_void,
    pub vsp_param_sz: size_t,
    pub vsp_error: *mut c_void,
    pub vsp_error_sz: size_t,
}

/// This is the viona interface version which viona_api expects to operate
/// against.  All constants and structs defined by the crate are done so in
/// terms of that specific version.
pub const VIONA_CURRENT_INTERFACE_VERSION: u32 = 6;

/// Maximum size of packed nvlists used in viona parameter ioctls
pub const VIONA_MAX_PARAM_NVLIST_SZ: usize = 4096;
