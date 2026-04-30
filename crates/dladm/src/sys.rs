// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use libc::{c_char, c_int, c_uchar, c_uint, c_void};
use num_enum::TryFromPrimitive;
use strum::FromRepr;
use strum::IntoStaticStr;

#[cfg(target_os = "illumos")]
#[link(name = "dladm")]
extern "C" {
    pub fn dladm_open(handle: *mut dladm_handle_t) -> c_int;
    pub fn dladm_close(handle: dladm_handle_t);
    pub fn dladm_name2info(
        handle: dladm_handle_t,
        link: *const c_char,
        linkidp: *mut datalink_id_t,
        flagp: *mut DlAdmOpt,
        classp: *mut datalink_class_t,
        mediap: *mut u32,
    ) -> c_int;
    pub fn dladm_walk_macaddr(
        handle: dladm_handle_t,
        linkid: datalink_id_t,
        arg: *mut c_void,
        callback: unsafe extern "C" fn(
            *mut c_void,
            *mut dladm_macaddr_attr_t,
        ) -> boolean_t,
    ) -> c_int;
    pub fn dladm_get_linkprop(
        handle: dladm_handle_t,
        linkid: datalink_id_t,
        prop_type: u32,
        prop_name: *const c_char,
        prop_val: *mut *mut c_char,
        val_cntp: *mut u32,
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
    pub unsafe extern "C" fn dladm_name2info(
        handle: dladm_handle_t,
        link: *const c_char,
        linkidp: *mut datalink_id_t,
        flagp: *mut DlAdmOpt,
        classp: *mut datalink_class_t,
        mediap: *mut u32,
    ) -> c_int {
        panic!("illumos only");
    }
    pub unsafe extern "C" fn dladm_walk_macaddr(
        handle: dladm_handle_t,
        linkid: datalink_id_t,
        arg: *mut c_void,
        callback: unsafe extern "C" fn(
            *mut c_void,
            *mut dladm_macaddr_attr_t,
        ) -> boolean_t,
    ) -> c_int {
        panic!("illumos only");
    }

    pub fn dladm_get_linkprop(
        handle: dladm_handle_t,
        linkid: datalink_id_t,
        prop_type: u32,
        prop_name: *const c_char,
        prop_val: *mut *mut c_char,
        val_cntp: *mut u32,
    ) -> c_int {
        panic!("illumos only");
    }
}

#[cfg(not(target_os = "illumos"))]
pub use compat::*;

/* opaque dladm handle to libdladm functions */
#[repr(C)]
pub struct dladm_handle {
    _data: (),
    _marker: core::marker::PhantomData<()>,
}

pub type dladm_handle_t = *mut dladm_handle;
pub type datalink_id_t = u32;

#[repr(C)]
pub struct dladm_macaddr_attr_t {
    pub ma_slot: c_uint,
    pub ma_flags: c_uint,
    pub ma_addr: [c_uchar; MAXMACADDRLEN],
    pub ma_addrlen: c_uint,
    pub ma_client_name: [c_char; MAXNAMELEN],
    pub ma_client_linkid: datalink_id_t,
}

#[repr(C)]
pub enum boolean_t {
    B_FALSE,
    B_TRUE,
}

const MAXMACADDRLEN: usize = 20;
const MAXNAMELEN: usize = 256;
pub(crate) const MAXLINKNAMELEN: usize = 32;

bitflags::bitflags! {
    /// Despite being mutually exclusive options,
    /// values of this type double as a mask in
    /// certain operations. It is very convenient
    /// to simply pretend it is a bitflag.
    #[derive(Copy, Clone, Default, Debug)]
    #[repr(transparent)]
    pub struct datalink_class_t: u32 {
        const DATALINK_CLASS_PHYS = 0x01;
        const DATALINK_CLASS_VLAN = 0x02;
        const DATALINK_CLASS_AGGR = 0x04;
        const DATALINK_CLASS_VNIC = 0x08;
        const DATALINK_CLASS_ETHERSTUB = 0x10;
        const DATALINK_CLASS_SIMNET = 0x20;
        const DATALINK_CLASS_BRIDGE = 0x40;
        const DATALINK_CLASS_IPTUN = 0x80;
        const DATALINK_CLASS_PART = 0x100;
        const DATALINK_CLASS_MISC = 0x400;
    }
}

bitflags::bitflags! {
    /// Annoyingly,`libdladm` does not define an enum for the various
    /// `DLADM_OPT_*` bitflags. We introduce our own here and the
    /// naming convention reflects that this is a Rust type
    /// that happens to be marshallable across an FFI boundary.
    #[derive(Copy, Clone, Default, Debug)]
    #[repr(transparent)]
    pub struct DlAdmOpt: u32 {
        /// The function requests to bringup some configuration that only takes
        /// effect on the active system (not persistent).
        const ACTIVE     = 0x00000001;

        /// The function requests to persist some configuration.
        const PERSIST    = 0x00000002;

        /// Today, only used by `dladm_set_secobj()` — requests to create a secobj.
        const CREATE     = 0x00000004;

        /// The function requests to execute a specific operation forcefully.
        const FORCE      = 0x00000008;

        /// The function requests to generate a link name using the specified prefix.
        const PREFIX     = 0x00000010;

        const ANCHOR     = 0x00000020;

        /// Signifies VLAN creation code path.
        const VLAN       = 0x00000040;

        /// Do not refresh the daemon after setting parameter (used by STP mcheck).
        const NOREFRESH  = 0x00000080;

        /// Bypass check functions during boot (used by pool property since pools
        /// can come up after link properties are set).
        const BOOT       = 0x00000100;

        /// Indicates that the link assigned to a zone is transient and will be
        /// removed when the zone shuts down.
        const TRANSIENT  = 0x00000200;
    }
}

/// DLPI media types (DL_* constants from `<sys/dlpi.h>`).
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, TryFromPrimitive)]
#[non_exhaustive]
pub enum DlMediaType {
    /// IEEE 802.3 CSMA/CD network.
    Csmacd = 0x0,
    /// IEEE 802.4 Token Passing Bus.
    Tpb = 0x1,
    /// IEEE 802.5 Token Passing Ring.
    Tpr = 0x2,
    /// IEEE 802.6 Metro Net.
    Metro = 0x3,
    /// Ethernet Bus.
    Ether = 0x4,
    /// ISO HDLC protocol support.
    Hdlc = 0x05,
    /// Character Synchronous protocol support.
    Char = 0x06,
    /// IBM Channel-to-Channel Adapter.
    Ctca = 0x07,
    /// Fiber Distributed Data Interface.
    Fddi = 0x08,
    /// Any other medium not listed above.
    Other = 0x09,
    /// Frame Relay LAPF.
    Frame = 0x0a,
    /// Multi-protocol over Frame Relay.
    MpFrame = 0x0b,
    /// Character Asynchronous Protocol.
    Async = 0x0c,
    /// X.25 Classical IP interface.
    IpX25 = 0x0d,
    /// Software loopback.
    Loop = 0x0e,
    /// Fibre Channel interface.
    Fc = 0x10,
    /// ATM.
    Atm = 0x11,
    /// ATM Classical IP interface.
    IpAtm = 0x12,
    /// X.25 LAPB interface.
    X25 = 0x13,
    /// ISDN interface.
    Isdn = 0x14,
    /// HIPPI interface.
    Hippi = 0x15,
    /// 100 Based VG Ethernet.
    Vg100 = 0x16,
    /// 100 Based VG Token Ring.
    Vg100Tpr = 0x17,
    /// ISO 8802/3 and Ethernet.
    EthCsma = 0x18,
    /// 100 Base T.
    Bt100 = 0x19,
    /// Infiniband.
    Ib = 0x1a,

    /// IPv4 Tunnel Link.
    Ipv4 = 0x8000_0001,
    /// IPv6 Tunnel Link.
    Ipv6 = 0x8000_0002,
    /// Virtual network interface.
    SunwVni = 0x8000_0003,
    /// IEEE 802.11.
    WiFi = 0x8000_0004,
    /// `ipnet(4D)` link.
    IpNet = 0x8000_0005,
    /// IPMP stub interface.
    SunwIpmp = 0x8000_0006,
    /// 6to4 Tunnel Link.
    SixToFour = 0x8000_0007,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, FromRepr, IntoStaticStr)]
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
