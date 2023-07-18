// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use libc::c_int;
use num_enum::TryFromPrimitive;

#[cfg(target_os = "illumos")]
#[link(name = "dladm")]
extern "C" {
    pub fn dladm_open(handle: *mut dladm_handle_t) -> c_int;
    pub fn dladm_close(handle: dladm_handle_t);
    pub fn dladm_name2info(
        handle: dladm_handle_t,
        link: *const u8,
        linkidp: *mut datalink_id_t,
        flagp: *mut u32,
        // parse to datalink_class
        classp: *mut c_int,
        mediap: *mut u32,
    ) -> c_int;
}

#[cfg(not(target_os = "illumos"))]
mod compat {
    #![allow(unused)]

    use super::*;

    pub unsafe extern "C" fn dladm_open(handle: *mut dladm_handle_t) -> c_int {
        panic!("illumos only");
    }
    pub unsafe extern "C" fn dladm_close(handle: dladm_handle_t) {
        panic!("illumos only");
    }
    #[cfg(not(target_os = "illumos"))]
    pub unsafe extern "C" fn dladm_name2info(
        handle: dladm_handle_t,
        link: *const u8,
        linkidp: *mut datalink_id_t,
        flagp: *mut u32,
        // parse to datalink_class
        classp: *mut c_int,
        mediap: *mut u32,
    ) -> c_int {
        panic!("illumos only");
    }
}
#[cfg(not(target_os = "illumos"))]
pub use compat::*;

/* opaque dladm handle to libdladm functions */
pub enum dladm_handle {}
pub type dladm_handle_t = *mut dladm_handle;
pub type datalink_id_t = u32;

#[derive(Copy, Clone, Debug, Eq, PartialEq, TryFromPrimitive)]
#[repr(i32)]
pub enum datalink_class {
    DATALINK_CLASS_PHYS = 0x01,
    DATALINK_CLASS_VLAN = 0x02,
    DATALINK_CLASS_AGGR = 0x04,
    DATALINK_CLASS_VNIC = 0x08,
    DATALINK_CLASS_ETHERSTUB = 0x10,
    DATALINK_CLASS_SIMNET = 0x20,
    DATALINK_CLASS_BRIDGE = 0x40,
    DATALINK_CLASS_IPTUN = 0x80,
    DATALINK_CLASS_PART = 0x100,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, TryFromPrimitive)]
#[repr(i32)]
pub enum dladm_status {
    DLADM_STATUS_OK = 0,
    DLADM_STATUS_BADARG,
    DLADM_STATUS_FAILED,
    DLADM_STATUS_TOOSMALL,
    DLADM_STATUS_NOTSUP,
    DLADM_STATUS_NOTFOUND,
    DLADM_STATUS_BADVAL,
    DLADM_STATUS_NOMEM,
    DLADM_STATUS_EXIST,
    DLADM_STATUS_LINKINVAL,
    DLADM_STATUS_PROPRDONLY,
    DLADM_STATUS_BADVALCNT,
    DLADM_STATUS_DBNOTFOUND,
    DLADM_STATUS_DENIED,
    DLADM_STATUS_IOERR,
    DLADM_STATUS_TEMPONLY,
    DLADM_STATUS_TIMEDOUT,
    DLADM_STATUS_ISCONN,
    DLADM_STATUS_NOTCONN,
    DLADM_STATUS_REPOSITORYINVAL,
    DLADM_STATUS_MACADDRINVAL,
    DLADM_STATUS_KEYINVAL,
    DLADM_STATUS_INVALIDMACADDRLEN,
    DLADM_STATUS_INVALIDMACADDRTYPE,
    DLADM_STATUS_LINKBUSY,
    DLADM_STATUS_VIDINVAL,
    DLADM_STATUS_NONOTIF,
    DLADM_STATUS_TRYAGAIN,
    DLADM_STATUS_IPTUNTYPE,
    DLADM_STATUS_IPTUNTYPEREQD,
    DLADM_STATUS_BADIPTUNLADDR,
    DLADM_STATUS_BADIPTUNRADDR,
    DLADM_STATUS_ADDRINUSE,
    DLADM_STATUS_BADTIMEVAL,
    DLADM_STATUS_INVALIDMACADDR,
    DLADM_STATUS_INVALIDMACADDRNIC,
    DLADM_STATUS_INVALIDMACADDRINUSE,
    DLADM_STATUS_MACFACTORYSLOTINVALID,
    DLADM_STATUS_MACFACTORYSLOTUSED,
    DLADM_STATUS_MACFACTORYSLOTALLUSED,
    DLADM_STATUS_MACFACTORYNOTSUP,
    DLADM_STATUS_INVALIDMACPREFIX,
    DLADM_STATUS_INVALIDMACPREFIXLEN,
    DLADM_STATUS_BADCPUID,
    DLADM_STATUS_CPUERR,
    DLADM_STATUS_CPUNOTONLINE,
    DLADM_STATUS_BADRANGE,
    DLADM_STATUS_TOOMANYELEMENTS,
    DLADM_STATUS_DB_NOTFOUND,
    DLADM_STATUS_DB_PARSE_ERR,
    DLADM_STATUS_PROP_PARSE_ERR,
    DLADM_STATUS_ATTR_PARSE_ERR,
    DLADM_STATUS_FLOW_DB_ERR,
    DLADM_STATUS_FLOW_DB_OPEN_ERR,
    DLADM_STATUS_FLOW_DB_PARSE_ERR,
    DLADM_STATUS_FLOWPROP_DB_PARSE_ERR,
    DLADM_STATUS_FLOW_ADD_ERR,
    DLADM_STATUS_FLOW_WALK_ERR,
    DLADM_STATUS_FLOW_IDENTICAL,
    DLADM_STATUS_FLOW_INCOMPATIBLE,
    DLADM_STATUS_FLOW_EXISTS,
    DLADM_STATUS_PERSIST_FLOW_EXISTS,
    DLADM_STATUS_INVALID_IP,
    DLADM_STATUS_INVALID_PREFIXLEN,
    DLADM_STATUS_INVALID_PROTOCOL,
    DLADM_STATUS_INVALID_PORT,
    DLADM_STATUS_INVALID_DSF,
    DLADM_STATUS_INVALID_DSFMASK,
    DLADM_STATUS_INVALID_MACMARGIN,
    DLADM_STATUS_NOTDEFINED,
    DLADM_STATUS_BADPROP,
    DLADM_STATUS_MINMAXBW,
    DLADM_STATUS_NO_HWRINGS,
    DLADM_STATUS_PERMONLY,
    DLADM_STATUS_OPTMISSING,
    DLADM_STATUS_POOLCPU,
    DLADM_STATUS_INVALID_PORT_INSTANCE,
    DLADM_STATUS_PORT_IS_DOWN,
    DLADM_STATUS_PKEY_NOT_PRESENT,
    DLADM_STATUS_PARTITION_EXISTS,
    DLADM_STATUS_INVALID_PKEY,
    DLADM_STATUS_NO_IB_HW_RESOURCE,
    DLADM_STATUS_INVALID_PKEY_TBL_SIZE,
    DLADM_STATUS_PORT_NOPROTO,
    DLADM_STATUS_INVALID_MTU,
    DLADM_STATUS_PERSIST_ON_TEMP,
}
