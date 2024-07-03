// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::ffi::CString;
use std::io::{BufRead, BufReader, Error, ErrorKind, Result};
use std::process::{Command, Stdio};
use std::ptr::addr_of_mut;

#[allow(non_camel_case_types)]
mod sys;

use libc::c_void;
use sys::{datalink_class, dladm_handle_t, dladm_status};

pub struct Handle {
    inner: dladm_handle_t,
}
impl Handle {
    pub fn new() -> Result<Self> {
        let mut hdl: dladm_handle_t = std::ptr::null_mut();
        Self::handle_dladm_err(unsafe {
            sys::dladm_open(&mut hdl as *mut dladm_handle_t)
        })?;
        Ok(Self { inner: hdl })
    }

    pub fn query_vnic(&self, name: &str) -> Result<LinkInfo> {
        let name_cstr = CString::new(name).unwrap();
        let mut link_id: sys::datalink_id_t = 0;
        let mut class: i32 = 0;
        Self::handle_dladm_err(unsafe {
            sys::dladm_name2info(
                self.inner,
                name_cstr.to_bytes_with_nul().as_ptr(),
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
                Self::get_vnic_mac(name, &mut res.mac_addr[..])?;
            }
            Some(datalink_class::DATALINK_CLASS_MISC) => {
                self.get_misc_mac(link_id, &mut res.mac_addr[..])?;
            }
            Some(c) => {
                return Err(Error::new(
                    ErrorKind::InvalidInput,
                    format!("{} is not vnic/misc class, but {:?}", name, c),
                ));
            }
            None => {
                return Err(Error::new(
                    ErrorKind::InvalidInput,
                    format!("{} is of invalid class {:x}", name, class),
                ));
            }
        }

        res.mtu = Self::get_mtu(name).ok();

        Ok(res)
    }
    fn get_mtu(name: &str) -> Result<u16> {
        // dladm show-linkprop -c -o value -p mtu <NIC_NAME>
        // 1500
        let output = Command::new("dladm")
            .args(["show-linkprop", "-c", "-o", "value", "-p", "mtu"])
            .arg(name)
            .stderr(Stdio::null())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .output()?;
        if !output.status.success() {
            return Err(Error::new(ErrorKind::Other, "failed dladm"));
        }
        BufReader::new(&output.stdout[..])
            .lines()
            .next()
            .and_then(Result::ok)
            .and_then(|line| line.parse::<u16>().ok())
            .ok_or_else(|| Error::new(ErrorKind::Other, "invalid mtu"))
    }
    fn get_vnic_mac(name: &str, mac: &mut [u8]) -> Result<()> {
        // dladm show-vnic -p -o macaddress <VNIC_NAME>
        // 2:8:20:2d:e9:24
        let output = Command::new("dladm")
            .args(["show-vnic", "-p", "-o", "macaddress"])
            .arg(name)
            .stderr(Stdio::null())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .output()?;
        if !output.status.success() {
            return Err(Error::new(ErrorKind::Other, "failed dladm"));
        }
        let addr = BufReader::new(&output.stdout[..])
            .lines()
            .next()
            .and_then(Result::ok)
            .and_then(|line| {
                let fields: Vec<u8> = line
                    .split(':')
                    .filter_map(|f| u8::from_str_radix(f, 16).ok())
                    .collect();
                match fields.len() {
                    ETHERADDRL => Some(fields),
                    _ => None,
                }
            })
            .ok_or_else(|| {
                Error::new(ErrorKind::Other, "cannot query mac addr")
            })?;
        mac.copy_from_slice(&addr[..]);
        Ok(())
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
                state.mac.copy_from_slice(&(*macaddr).ma_addr[..ETHERADDRL]);
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
                self.inner,
                linkid,
                &mut state as *mut _ as *mut c_void,
                per_macaddr,
            )
        })?;

        if state.n_seen == 0 {
            return Err(Error::new(
                ErrorKind::Other,
                "no mac addrs found on link",
            ));
        } else if !state.written {
            return Err(Error::new(
                ErrorKind::Other,
                "no mac addrs on link had correct length (6B)",
            ));
        }

        Ok(())
    }

    fn handle_dladm_err(v: i32) -> Result<()> {
        match dladm_status::from_repr(v)
            .unwrap_or(dladm_status::DLADM_STATUS_FAILED)
        {
            dladm_status::DLADM_STATUS_OK => Ok(()),
            e => Err(Error::new(ErrorKind::Other, format!("{:?}", e))),
        }
    }
}
impl Drop for Handle {
    fn drop(&mut self) {
        unsafe { sys::dladm_close(self.inner) }
        self.inner = std::ptr::null_mut();
    }
}

const ETHERADDRL: usize = 6;

#[derive(Copy, Clone, Default)]
pub struct LinkInfo {
    pub link_id: u32,
    pub mtu: Option<u16>,
    pub mac_addr: [u8; ETHERADDRL],
}
