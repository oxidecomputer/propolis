// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

extern crate alloc;

use core::cell::UnsafeCell;
use core::ptr::NonNull;
use core::slice;

#[allow(non_camel_case_types)]
mod sys;

use libc::c_void;
use sys::{
    datalink_class, dladm_handle, dladm_handle_t, dladm_status, MAXLINKNAMELEN,
};

pub type Result<T> = core::result::Result<T, DladmError>;

#[derive(Debug)]
pub enum DladmError {
    DladmSubsystem(dladm_status),
    UnexpectedClass(datalink_class),
    InvalidClass,
    Other,
}

/// Rust-flavoured wrapper around `libdladm`.
/// Brokers access to dladm without execing the dladm CLI.
pub struct Dladm {
    inner: NonNull<dladm_handle>,
}

impl Dladm {
    /// Open a handle to the `dladm` subsystem.
    pub fn new() -> Result<Self> {
        let mut hdl: *mut dladm_handle = std::ptr::null_mut();
        Self::handle_dladm_err(unsafe { sys::dladm_open(&mut hdl) })?;

        debug_assert!(!hdl.is_null());

        Ok(Self { inner: unsafe { NonNull::new_unchecked(hdl) } })
    }

    pub fn query_link(&self, name: &str) -> Result<LinkInfo> {
        let mut stack_alloc_link_name = [0; MAXLINKNAMELEN];

        for (index, copyme) in name.bytes().take(MAXLINKNAMELEN - 1).enumerate()
        {
            stack_alloc_link_name[index] = copyme;
        }

        let mut link_id: sys::datalink_id_t = 0;
        let mut class: i32 = 0;
        Self::handle_dladm_err(unsafe {
            sys::dladm_name2info(
                self.inner.as_ptr(),
                stack_alloc_link_name.as_ptr(),
                &mut link_id as *mut sys::datalink_id_t,
                std::ptr::null_mut(),
                &mut class,
                std::ptr::null_mut(),
            )
        })?;

        let mut res = LinkInfo { link_id, ..Default::default() };

        match datalink_class::from_repr(class) {
            // acceptable values: this supports both VNICs
            // and direct use of XDE/OPTE ports.
            Some(datalink_class::DATALINK_CLASS_VNIC) => {
                //Self::get_vnic_mac(name, &mut res.mac_addr[..])?;
            }
            Some(datalink_class::DATALINK_CLASS_MISC) => {
                self.get_misc_mac(link_id, &mut res.mac_addr[..])?;
            }
            Some(c) => {
                return Err(DladmError::UnexpectedClass(c));
            }
            None => return Err(DladmError::InvalidClass),
        }

        //res.mtu = Self::get_mtu(name).ok();

        Ok(res)
    }

    fn get_misc_mac(
        &self,
        linkid: sys::datalink_id_t,
        mac: &mut [u8],
    ) -> Result<()> {
        // Unfortunately, XDE/OPTE creates 'misc' type devices, as it is
        // a pseudo device. `dladm` has no built-in commands for these,
        // and macaddr queries for all other link types go through their
        // dedicated `dladm show-<X>` commands. As a consequence, we have
        // to go to libdladm/libdllink directly here.

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

#[derive(Debug, Copy, Clone, Default)]
pub struct LinkInfo {
    pub link_id: u32,
    pub mtu: Option<u16>,
    pub mac_addr: [u8; ETHERADDRL],
}
