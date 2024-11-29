// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

#![allow(clippy::new_without_default)]

use nvpair_sys::*;

use std::ffi::CStr;
use std::ptr::NonNull;

pub struct NvList(NonNull<nvlist_t>);

impl NvList {
    pub fn new() -> Self {
        unsafe {
            let nvlp = fnvlist_alloc();
            Self(NonNull::new_unchecked(nvlp))
        }
    }
    pub fn pack(&mut self) -> Packed {
        unsafe {
            let mut size = 0;
            let ptr = fnvlist_pack(self.0.as_mut(), &mut size);
            Packed { data: NonNull::new_unchecked(ptr.cast()), size }
        }
    }

    pub fn unpack(buf: &mut [u8]) -> std::io::Result<Self> {
        Self::unpack_ptr(buf.as_mut_ptr(), buf.len())
    }

    pub fn unpack_ptr(buf: *mut u8, len: usize) -> std::io::Result<Self> {
        let mut nvp = std::ptr::null_mut();
        match unsafe { nvlist_unpack(buf.cast(), len, &mut nvp, 0) } {
            0 => Ok(Self(
                NonNull::new(nvp)
                    .expect("nvlist_unpack emits non-NULL pointer on success"),
            )),
            err => Err(std::io::Error::from_raw_os_error(err)),
        }
    }

    #[inline(always)]
    pub fn add<'a>(
        &'a mut self,
        name: impl Into<NvName<'a>>,
        value: impl Into<NvData<'a>>,
    ) {
        self.add_name_value(name.into(), value.into());
    }

    pub fn add_name_value(&mut self, name: NvName, value: NvData) {
        unsafe {
            let name = name.as_ptr();
            let nvlp = self.0.as_mut();

            match value {
                NvData::Boolean => {
                    fnvlist_add_boolean(nvlp, name);
                }
                NvData::BooleanValue(val) => {
                    fnvlist_add_boolean_value(nvlp, name, val.into());
                }
                NvData::Byte(val) => {
                    fnvlist_add_byte(nvlp, name, val);
                }
                NvData::Int8(val) => {
                    fnvlist_add_int8(nvlp, name, val);
                }
                NvData::UInt8(val) => {
                    fnvlist_add_uint8(nvlp, name, val);
                }
                NvData::Int16(val) => {
                    fnvlist_add_int16(nvlp, name, val);
                }
                NvData::UInt16(val) => {
                    fnvlist_add_uint16(nvlp, name, val);
                }
                NvData::Int32(val) => {
                    fnvlist_add_int32(nvlp, name, val);
                }
                NvData::UInt32(val) => {
                    fnvlist_add_uint32(nvlp, name, val);
                }
                NvData::Int64(val) => {
                    fnvlist_add_int64(nvlp, name, val);
                }
                NvData::UInt64(val) => {
                    fnvlist_add_uint64(nvlp, name, val);
                }
                NvData::NvList(val) => {
                    // SAFETY: while this takes a *mut nvlist_t, we are counting
                    // on libnvpair to not actually mutate the to-be-added list.
                    fnvlist_add_nvlist(nvlp, name, val.0.as_ptr());
                }
                NvData::String(val) => {
                    fnvlist_add_string(nvlp, name, val.as_ptr());
                }
            }
        }
    }
}
impl Drop for NvList {
    fn drop(&mut self) {
        unsafe {
            fnvlist_free(self.0.as_mut());
        }
    }
}

pub struct Packed {
    data: NonNull<u8>,
    size: usize,
}
impl Packed {
    pub fn as_ptr(&self) -> *const u8 {
        self.data.as_ptr()
    }
    pub fn as_mut_ptr(&mut self) -> *mut u8 {
        self.data.as_ptr()
    }
}
impl AsRef<[u8]> for Packed {
    fn as_ref(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.data.as_ptr(), self.size) }
    }
}
impl Drop for Packed {
    fn drop(&mut self) {
        unsafe { fnvlist_pack_free(self.data.as_ptr().cast(), self.size) }
    }
}

macro_rules! nvdata_from {
    (&$l:lifetime $t:ty, $i:ident) => {
        impl<$l> From<& $l $t> for NvData<$l> {
            fn from(value: & $l $t) -> Self {
                Self::$i(value)
            }
        }
    };
    ($t:ty, $i:ident) => {
        impl From<$t> for NvData<'_> {
            fn from(value: $t) -> Self {
                Self::$i(value)
            }
        }
    };
}

pub enum NvData<'a> {
    Boolean,
    BooleanValue(bool),
    Byte(u8),
    Int8(i8),
    UInt8(u8),
    Int16(i16),
    UInt16(u16),
    Int32(i32),
    UInt32(u32),
    Int64(i64),
    UInt64(u64),
    NvList(&'a NvList),
    String(&'a CStr),
}

nvdata_from!(bool, BooleanValue);
nvdata_from!(i8, Int8);
nvdata_from!(u8, UInt8);
nvdata_from!(i16, Int16);
nvdata_from!(u16, UInt16);
nvdata_from!(i32, Int32);
nvdata_from!(u32, UInt32);
nvdata_from!(i64, Int64);
nvdata_from!(u64, UInt64);
nvdata_from!(&'a CStr, String);

pub enum NvName<'a> {
    Owned(Vec<u8>),
    Loaned(&'a [u8]),
}
impl NvName<'_> {
    pub fn as_ptr(&self) -> *const i8 {
        match self {
            NvName::Owned(v) => v.as_ptr().cast(),
            NvName::Loaned(s) => s.as_ptr().cast(),
        }
    }
}
impl AsRef<[u8]> for NvName<'_> {
    fn as_ref(&self) -> &[u8] {
        match self {
            NvName::Owned(b) => b.as_slice(),
            NvName::Loaned(s) => s,
        }
    }
}
impl Clone for NvName<'_> {
    fn clone(&self) -> Self {
        match self {
            NvName::Owned(v) => NvName::Owned(v.clone()),
            NvName::Loaned(s) => NvName::Owned(s.to_vec()),
        }
    }
}
impl<'a> From<&'a str> for NvName<'a> {
    fn from(value: &'a str) -> Self {
        let bytes = value.as_bytes();
        if let Some(nul_idx) =
            bytes.iter().enumerate().find_map(|(idx, b)| match *b {
                0 => Some(idx),
                _ => None,
            })
        {
            Self::Loaned(&bytes[..=nul_idx])
        } else {
            let mut copy = Vec::with_capacity(bytes.len() + 1);
            copy.extend(bytes);
            copy.push(0);
            Self::Owned(copy)
        }
    }
}
impl<'a> From<&'a CStr> for NvName<'a> {
    fn from(value: &'a CStr) -> Self {
        Self::Loaned(value.to_bytes())
    }
}
