// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Ultra-low overhead (no_std, no_alloc) access to the dladm subsystem.

#![no_std]

use core::ffi::CStr;
use core::ptr::NonNull;
use core::slice;
use libc::c_void;
use sys::{
    datalink_class_t, datalink_id_t, dladm_handle, dladm_handle_t,
    dladm_status, DlAdmOpt, DlMediaType, MAXLINKNAMELEN,
};

#[allow(non_camel_case_types)]
mod sys;

pub type Result<T> = core::result::Result<T, DladmError>;

#[derive(Debug)]
pub enum DladmError {
    DladmSubsystem(dladm_status),
    InvalidClass,
    Other,

    /// A `&str` the caller provided is not a valid link name.
    InvalidLinkName,

    /// Either `libdladm` has a bug or our expectations were wrong.
    UnexpectedNullPtr,
}

/// Rust-flavoured wrapper around `libdladm`.
/// Brokers access to dladm without execing the dladm CLI.
/// Ultimately `libdladm` communicates over doors with `dlmgmtd`,
/// a daemon managed by SMF's `svc:/network/datalink-management:default`.
pub struct Dladm {
    inner: NonNull<dladm_handle>,
}

impl Dladm {
    /// Open a handle to the `dladm` subsystem.
    pub fn new() -> Result<Self> {
        let mut hdl: *mut dladm_handle = core::ptr::null_mut();
        Self::handle_dladm_err(unsafe { sys::dladm_open(&mut hdl) })?;

        let Some(ptr) = NonNull::new(hdl) else {
            return Err(DladmError::UnexpectedNullPtr);
        };

        Ok(Self { inner: ptr })
    }

    pub fn describe_link(&self, name: &str) -> Result<LinkInfo> {
        let nb = name.as_bytes();

        const SPACE_NEEDED_FOR_NULL_TERMINATOR: usize = 1;

        if nb.is_empty()
            || nb.len() + SPACE_NEEDED_FOR_NULL_TERMINATOR > MAXLINKNAMELEN
        {
            return Err(DladmError::InvalidLinkName);
        }

        let mut buf = [0u8; MAXLINKNAMELEN];
        let n = nb.len().min(MAXLINKNAMELEN - SPACE_NEEDED_FOR_NULL_TERMINATOR);
        buf[..n].copy_from_slice(&nb[..n]);

        let Ok(link_name) = CStr::from_bytes_until_nul(&buf) else {
            return Err(DladmError::InvalidLinkName);
        };

        let mut link_id = 0;
        let mut class = datalink_class_t::empty();
        let mut flags = DlAdmOpt::empty();
        let mut media = 0;

        // SAFETY: All pointers we're passing to libdladm are valid
        // for their upcoming accesses.
        // The link name is a stack-allocated C-string that we've
        // ensured contains exactly one null terminator.
        Self::handle_dladm_err(unsafe {
            sys::dladm_name2info(
                self.inner.as_ptr(),
                link_name.as_ptr(),
                &mut link_id,
                &mut flags,
                &mut class,
                &mut media,
            )
        })?;

        let media = DlpiMediaType::new(media);

        let mut res = LinkInfo {
            link_id,
            class,
            flags,
            media,
            mtu: Some(0),
            mac_addr: [0; 6],
        };

        self.yoink_first_mac(res.link_id, &mut res.mac_addr);

        let mut buffer = [0; 256];
        let mut len = 1u32;

        Self::handle_dladm_err(unsafe {
            sys::dladm_get_linkprop(
                self.inner.as_ptr(),
                res.link_id,
                1,
                MTU_PROP_NAME.as_ptr(),
                &mut buffer.as_mut_ptr(),
                &mut len,
            )
        })?;

        panic!("{:#?}", buffer);

        Ok(res)
    }

    fn yoink_first_mac(
        &self,
        linkid: sys::datalink_id_t,
        mac: &mut [u8],
    ) -> Result<()> {
        // One-off callback function and arg struct.
        // This will use the first seen mac address attached to the link.
        unsafe extern "C" fn per_macaddr(
            arg: *mut c_void,
            macaddr: *mut sys::dladm_macaddr_attr_t,
        ) -> sys::boolean_t {
            let state = &mut *(arg as *mut Arg);
            state.n_seen += 1;

            if (*macaddr).ma_addrlen == (ETHERADDRL as u32) {
                let ma_addr = slice::from_raw_parts(
                    &raw const (*macaddr).ma_addr as *const u8,
                    ETHERADDRL,
                );
                state.mac.copy_from_slice(ma_addr);
                state.written = true;
                sys::boolean_t::B_FALSE
            } else {
                // Keep going.
                sys::boolean_t::B_TRUE
            }
        }

        struct Arg<'a> {
            mac: &'a mut [u8],
            n_seen: usize,
            written: bool,
        }

        let mut state = Arg { mac, n_seen: 0, written: false };

        // SAFETY: dladm_handle_t is known to be valid, and &mut reference
        // to state is only held inside the callback.
        Self::handle_dladm_err(unsafe {
            sys::dladm_walk_macaddr(
                self.inner.as_ptr(),
                linkid,
                &mut state as *mut _ as *mut c_void,
                per_macaddr,
            )
        })?;

        if state.n_seen == 0 {
            return Err(DladmError::Other);
        } else if !state.written {
            return Err(DladmError::Other);
        }

        Ok(())
    }

    fn handle_dladm_err(v: i32) -> Result<()> {
        match dladm_status::from_repr(v)
            .unwrap_or(dladm_status::DLADM_STATUS_FAILED)
        {
            dladm_status::DLADM_STATUS_OK => Ok(()),
            e => Err(DladmError::DladmSubsystem(e)),
        }
    }
}

impl Drop for Dladm {
    fn drop(&mut self) {
        // Recall that dladm_handle_t is not just a close-able fd.
        // dladm_close performs the necessary cleanups.
        unsafe { sys::dladm_close(self.inner.as_mut()) }
    }
}

const ETHERADDRL: usize = 6;

static MTU_PROP_NAME:  &CStr = c"mtu";

#[derive(Debug, Copy, Clone)]
pub enum DlpiMediaType {
    Known(DlMediaType),

    /// Almost certainly this variant will never be constructed.
    /// We include it for guaranteed forwards-compatibility.
    Unknown(u32),
}

impl DlpiMediaType {
    fn new(value: u32) -> Self {
        match DlMediaType::try_from(value) {
            Ok(d) => Self::Known(d),
            _ => Self::Unknown(value),
        }
    }
}

#[derive(Debug, Copy, Clone)]
pub struct LinkInfo {
    pub link_id: u32,
    pub mtu: Option<u16>,
    pub mac_addr: [u8; ETHERADDRL],
    pub class: datalink_class_t,
    pub flags: DlAdmOpt,
    pub media: DlpiMediaType,
}
