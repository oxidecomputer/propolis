// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

#![allow(non_camel_case_types, non_snake_case)]

use std::ffi::{c_char, c_int};

use libc::size_t;

#[derive(Copy, Clone, Eq, PartialEq, Debug)]
#[repr(i32)]
pub enum data_type_t {
    DATA_TYPE_DONTCARE = -1,
    DATA_TYPE_UNKNOWN = 0,
    DATA_TYPE_BOOLEAN,
    DATA_TYPE_BYTE,
    DATA_TYPE_INT16,
    DATA_TYPE_UINT16,
    DATA_TYPE_INT32,
    DATA_TYPE_UINT32,
    DATA_TYPE_INT64,
    DATA_TYPE_UINT64,
    DATA_TYPE_STRING,
    DATA_TYPE_BYTE_ARRAY,
    DATA_TYPE_INT16_ARRAY,
    DATA_TYPE_UINT16_ARRAY,
    DATA_TYPE_INT32_ARRAY,
    DATA_TYPE_UINT32_ARRAY,
    DATA_TYPE_INT64_ARRAY,
    DATA_TYPE_UINT64_ARRAY,
    DATA_TYPE_STRING_ARRAY,
    DATA_TYPE_HRTIME,
    DATA_TYPE_NVLIST,
    DATA_TYPE_NVLIST_ARRAY,
    DATA_TYPE_BOOLEAN_VALUE,
    DATA_TYPE_INT8,
    DATA_TYPE_UINT8,
    DATA_TYPE_BOOLEAN_ARRAY,
    DATA_TYPE_INT8_ARRAY,
    DATA_TYPE_UINT8_ARRAY,
}

#[derive(Copy, Clone, Debug)]
#[repr(C)]
pub struct nvpair_t {
    pub nvp_size: i32,
    pub nvp_name_sz: i16,
    pub nvp_reserve: i16,
    pub nvp_value_elem: i32,
    pub nvp_type: i32,
}
impl nvpair_t {
    /// Get the name string from an `nvpair_t` pointer
    ///
    /// # Safety
    /// The `nvpair_t` pointer must be allocated from libnvpair to ensure
    /// expected positioning of name data.
    pub const unsafe fn NVP_NAME(nvp: *mut nvpair_t) -> *mut c_char {
        nvp.add(1).cast()
    }
    /// Get the value address from an `nvpair_t` pointer
    ///
    /// # Safety
    /// The `nvpair_t` pointer must be allocated from libnvpair to ensure
    /// expected positioning of value data.
    pub unsafe fn NVP_VALUE(nvp: *mut nvpair_t) -> *mut c_char {
        let name_sz = (*nvp).nvp_name_sz;

        NV_ALIGN(nvp.add(1) as usize + name_sz as usize) as *mut c_char
    }
}

#[derive(Copy, Clone, Debug)]
#[repr(C)]
pub struct nvlist_t {
    pub nvl_version: i32,
    pub nvl_nvflag: u32,
    pub nvl_priv: u64,
    pub nvl_flag: u32,
    pub nvl_pad: i32,
}

pub const NV_VERSION: i32 = 0;

pub const NV_ENCODE_NATIVE: u32 = 0;
pub const NV_ENCODE_XDR: u32 = 1;

pub const NV_UNIQUE_NAME: u32 = 0x1;
pub const NV_UNIQUE_NAME_TYPE: u32 = 0x2;

pub const NV_FLAG_NOENTOK: u32 = 0x1;

pub const fn NV_ALIGN(addr: usize) -> usize {
    (addr + 7) & !7usize
}

#[repr(C)]
pub enum boolean_t {
    B_FALSE = 0,
    B_TRUE = 1,
}
impl From<bool> for boolean_t {
    fn from(value: bool) -> Self {
        match value {
            false => boolean_t::B_FALSE,
            _ => boolean_t::B_TRUE,
        }
    }
}
impl From<boolean_t> for bool {
    fn from(value: boolean_t) -> Self {
        match value {
            boolean_t::B_FALSE => false,
            _ => false,
        }
    }
}

#[cfg_attr(target_os = "illumos", link(name = "nvpair"))]
extern "C" {
    pub fn nvlist_remove(
        nvl: *mut nvlist_t,
        name: *const c_char,
        dtype: data_type_t,
    ) -> c_int;
    pub fn nvlist_remove_all(nvl: *mut nvlist_t, name: *const c_char) -> c_int;
    pub fn nvlist_remove_nvpair(
        nvl: *mut nvlist_t,
        nvp: *mut nvpair_t,
    ) -> c_int;
    pub fn nvlist_lookup_nvpair(
        nvl: *mut nvlist_t,
        name: *const c_char,
        nvp: *mut *mut nvpair_t,
    ) -> c_int;

    pub fn nvlist_next_nvpair(
        nvl: *mut nvlist_t,
        nvp: *mut nvpair_t,
    ) -> *mut nvpair_t;
    pub fn nvlist_prev_nvpair(
        nvl: *mut nvlist_t,
        nvp: *mut nvpair_t,
    ) -> *mut nvpair_t;

    pub fn nvlist_exists(nvl: *mut nvlist_t, nvp: *const c_char) -> boolean_t;
    pub fn nvlist_empty(nvl: *mut nvlist_t) -> boolean_t;

    pub fn nvlist_unpack(
        buf: *mut c_char,
        size: size_t,
        nvlp: *mut *mut nvlist_t,
        flags: c_int,
    ) -> c_int;

    pub fn fnvlist_alloc() -> *mut nvlist_t;
    pub fn fnvlist_free(nvl: *mut nvlist_t);
    pub fn fnvlist_size(nvl: *mut nvlist_t) -> size_t;
    pub fn fnvlist_pack(nvl: *mut nvlist_t, sizep: *mut size_t) -> *mut c_char;
    pub fn fnvlist_pack_free(packed: *mut c_char, size: size_t);
    pub fn fnvlist_unpack(buf: *mut c_char, size: size_t) -> *mut nvlist_t;
    pub fn fnvlist_dup(nvl: *mut nvlist_t) -> *mut nvlist_t;
    pub fn fnvlist_merge(dst_nvl: *mut nvlist_t, src_nvl: *mut nvlist_t);
    pub fn fnvlist_num_pairs(nvl: *mut nvlist_t) -> size_t;

    pub fn fnvlist_add_boolean(nvl: *mut nvlist_t, name: *const c_char);
    pub fn fnvlist_add_boolean_value(
        nvl: *mut nvlist_t,
        name: *const c_char,
        val: boolean_t,
    );
    pub fn fnvlist_add_byte(nvl: *mut nvlist_t, name: *const c_char, val: u8);
    pub fn fnvlist_add_int8(nvl: *mut nvlist_t, name: *const c_char, val: i8);
    pub fn fnvlist_add_uint8(nvl: *mut nvlist_t, name: *const c_char, val: u8);
    pub fn fnvlist_add_int16(nvl: *mut nvlist_t, name: *const c_char, val: i16);
    pub fn fnvlist_add_uint16(
        nvl: *mut nvlist_t,
        name: *const c_char,
        val: u16,
    );
    pub fn fnvlist_add_int32(nvl: *mut nvlist_t, name: *const c_char, val: i32);
    pub fn fnvlist_add_uint32(
        nvl: *mut nvlist_t,
        name: *const c_char,
        val: u32,
    );
    pub fn fnvlist_add_int64(nvl: *mut nvlist_t, name: *const c_char, val: i64);
    pub fn fnvlist_add_uint64(
        nvl: *mut nvlist_t,
        name: *const c_char,
        val: u64,
    );
    pub fn fnvlist_add_string(
        nvl: *mut nvlist_t,
        name: *const c_char,
        val: *const c_char,
    );
    pub fn fnvlist_add_nvlist(
        nvl: *mut nvlist_t,
        name: *const c_char,
        val: *mut nvlist_t,
    );
    pub fn fnvlist_add_nvpair(nvl: *mut nvlist_t, val: *mut nvpair_t);
    // TODO: add_*_array functions
}
