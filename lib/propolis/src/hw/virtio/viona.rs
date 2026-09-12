// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

#![cfg_attr(not(target_os = "illumos"), allow(dead_code, unused_imports))]

use std::io::{self, Error, ErrorKind};
use std::num::NonZeroU16;
use std::os::unix::io::{AsRawFd, RawFd};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, Weak};

use crate::common::{RWOp, ReadOp};
use crate::hw::pci;
use crate::hw::virtio;
use crate::hw::virtio::queue::Chain;
use crate::lifecycle::{self, IndicatedState, Lifecycle};
use crate::migrate::{
    MigrateCtx, MigrateMulti, MigrateStateError, Migrator, PayloadOffers,
    PayloadOutputs,
};
use crate::util::regmap::RegMap;
use crate::vmm::{MemCtx, VmmHdl};

use super::bits::*;
use super::pci::{PciVirtio, PciVirtioState};
use super::queue::{self, VirtQueue, VirtQueues, VqSize};
use super::{VirtioDevice, VqChange, VqIntr};

use bit_field::BitField;
use lazy_static::lazy_static;
use serde::{Deserialize, Serialize};
use tokio::io::unix::AsyncFd;
use tokio::io::Interest;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use zerocopy::{FromBytes, Immutable, IntoBytes};

// Re-export API versioning interface for convenience of propolis consumers
pub use viona_api::{api_version, ApiVersion};

use viona_api::ETHERADDRL;

pub const RX_QUEUE_SIZE: VqSize = VqSize::new(0x800);
pub const TX_QUEUE_SIZE: VqSize = VqSize::new(0x100);
pub const CTL_QUEUE_SIZE: VqSize = VqSize::new(32);

pub const VIRTIO_MQ_MIN_QPAIRS: u16 = 1;
pub const VIRTIO_MQ_MAX_QPAIRS: u16 = 0x8000;

pub const PROPOLIS_MAX_MQ_PAIRS: u16 = 11;

pub const fn max_num_queues() -> usize {
    PROPOLIS_MAX_MQ_PAIRS as usize * 2
}

/// The index of the control queue when multiqueue (`VIRTIO_NET_F_MQ`) has
/// not been negotiated.
///
/// In this case, the driver will behave as though we have allocated only one
/// Rx/Tx queue pair, followed by the control queue.
pub const VIRTIO_NO_MQ_CTRL_Q_INDEX: usize = 2;

/// The caller of `set_use_pairs` will probably be inlined into a larger
/// function that is difficult to spot in a ustack(). This gives us a hint
/// about why we were `set_usepairs()`'ing.
#[repr(u8)]
enum MqSetPairsCause {
    Reset = 0,
    MqEnabled = 1,
    Commanded = 2,
    Import = 3,
}

/// Failure modes of `VNA_IOC_SET_MAC_FILTERS`.
#[derive(Debug, thiserror::Error)]
enum MacFilterError {
    /// The table exceeds the device capacity, which the kernel reports back
    /// through `vmf_nmcast`.
    #[error("VMF_ERR_COUNT (capacity {capacity})")]
    Count { capacity: u32 },
    #[error("VMF_ERR_NOT_MCAST for {addr:02x?}")]
    NotMulticast { addr: [u8; ETHERADDRL] },
    #[error("VMF_ERR_INSTALL for {addr:02x?}")]
    Install { addr: [u8; ETHERADDRL] },
    #[error("VMF_ERR_NO_UNICAST (no primary unicast address)")]
    NoUnicast,
    #[error("unknown MAC filter error code {0}")]
    Unknown(u32),
    #[error("VNA_IOC_SET_MAC_FILTERS failed: {0}")]
    Io(#[source] io::Error),
}

impl MacFilterError {
    fn probe_args(&self, requested: u32) -> (u32, u64, u32, i32) {
        let (code, addr) = match self {
            Self::Count { capacity } => {
                return (viona_api::VMF_ERR_COUNT, 0, *capacity, 0);
            }
            Self::NotMulticast { addr } => (viona_api::VMF_ERR_NOT_MCAST, addr),
            Self::Install { addr } => (viona_api::VMF_ERR_INSTALL, addr),
            Self::NoUnicast => {
                return (viona_api::VMF_ERR_NO_UNICAST, 0, requested, 0);
            }
            Self::Unknown(code) => return (*code, 0, requested, 0),
            Self::Io(error) => {
                return (
                    viona_api::VMF_OK,
                    0,
                    requested,
                    error.raw_os_error().unwrap_or(0),
                );
            }
        };
        let mut encoded = [0u8; size_of::<u64>()];
        encoded[size_of::<u64>() - ETHERADDRL..].copy_from_slice(addr);
        (code, u64::from_be_bytes(encoded), requested, 0)
    }
}

#[derive(Debug, thiserror::Error)]
enum RxConfigError {
    #[error("could not install multicast MAC filters: {0}")]
    InstallMacFilters(#[source] MacFilterError),
    #[error("could not clear multicast MAC filters: {0}")]
    ClearMacFilters(#[source] MacFilterError),
    #[error(
        "could not set promiscuity from {previous:?} to {requested:?}: {source}"
    )]
    SetPromisc {
        previous: PromiscLevel,
        requested: PromiscLevel,
        source: io::Error,
    },
    #[error("Rx configuration previously failed; device reset required")]
    NeedsReset,
    #[error("{primary} (promiscuous fallback also failed: {fallback})")]
    FallbackPromisc {
        primary: Box<RxConfigError>,
        fallback: Box<RxConfigError>,
    },
}

/// What caused Rx reconciliation of the multicast table.
enum RxReconcileCause {
    /// Guest filter state has changed, but the table did not.
    FilterChange,
    /// The guest or a migration source replaced the table.
    TableReplaced,
    /// Re-establish a known kernel state.
    Reinitialize,
}

/// The set of possible multicast table actions.
#[derive(Debug, Eq, PartialEq)]
enum McastTableAction {
    Install,
    Clear,
    Keep,
}

impl McastTableAction {
    /// Derive the appropriate kernel table operation needed to reconcile
    /// installation with the `wanted` table state from `RxConfig`:
    ///
    /// - `TableReplaced` forces reinstall even when the local state
    ///   has a table installed
    /// - `Reinitialize` clears the table even when local state is absent.
    /// - `FilterChange` keeps the existing table as is, unless `wanted`
    ///   and `installed` differ.
    fn derive(wanted: bool, installed: bool, cause: &RxReconcileCause) -> Self {
        use RxReconcileCause::{FilterChange, Reinitialize, TableReplaced};

        match (wanted, installed, cause) {
            (_, _, Reinitialize) => Self::Clear,
            (true, false, FilterChange | TableReplaced)
            | (true, true, TableReplaced) => Self::Install,
            (false, true, FilterChange | TableReplaced) => Self::Clear,
            (true, true, FilterChange)
            | (false, false, FilterChange | TableReplaced) => Self::Keep,
        }
    }
}

#[usdt::provider(provider = "propolis")]
mod probes {
    fn virtio_viona_mq_set_use_pairs(cause: u8, npairs: u16) {}
    fn virtio_viona_cq_request(class: u8, command: u8) {}
    /// A failed `VNA_IOC_SET_MAC_FILTERS`, with the semantic code from
    /// `vmf_err`, the offending address, the requested entry count,
    /// and the ioctl `errno`. On `VMF_ERR_COUNT`, `vmf_nmcast` contains
    /// the device's filter capacity.
    ///
    /// On ioctl failure, `err` is `VMF_OK` and `addr` is zero.
    /// `nmcast` retains the requested count.
    ///
    /// The `addr` arg encompasses the six-byte `vmf_erraddr`, right-aligned
    /// and encoded as a BE `u64`.
    fn virtio_viona_mac_filters_err(
        err: u32,
        addr: u64,
        nmcast: u32,
        errno: i32,
    ) {
    }
    /// A failed `VNA_IOC_SET_PROMISC`, with the requested level, the level
    /// recorded before the attempt, and the `errno`.
    fn virtio_viona_promisc_err(req: u8, prev: u8, err: i32) {}
    /// A multicast table too large to install, with the requested entry count
    /// and the capacity the kernel reported. Delivery remains at least
    /// all-multicast.
    fn virtio_viona_mac_filters_overflow(requested: u32, capacity: u32) {}
}

/// Types and so forth for supporting the control queue.
/// Note that these come from the VirtIO spec, section
/// 5.1.6.2 in VirtIO 1.2.
pub mod control {
    use super::MacAddr;
    use std::convert::TryFrom;
    use zerocopy::{FromBytes, IntoBytes};

    /// The control message header has two data: a u8 representing the "class"
    /// of control message, which describes what the message applies to, and a
    /// "command", which describes what action we should take in response to the
    /// command. So for example, class Mq and command Set means to set the
    /// number of multiqueue queue pairs.
    #[derive(Clone, Copy, Debug, Default, FromBytes)]
    #[repr(C)]
    pub struct Header {
        pub class: u8,
        pub command: u8,
    }

    #[derive(Clone, Copy, Debug)]
    pub enum Command {
        Rx(RxCmd),
        Mac(MacCmd),
        Vlan(VlanCmd),
        Announce(AnnounceCmd),
        Mq(MqCmd),
    }

    impl TryFrom<Header> for Command {
        type Error = Header;
        fn try_from(header: Header) -> Result<Self, Self::Error> {
            match (header.class, header.command) {
                (0, c) => Ok(Self::Rx(RxCmd::from_repr(c).ok_or(header)?)),
                (1, c) => Ok(Self::Mac(MacCmd::from_repr(c).ok_or(header)?)),
                (2, c) => Ok(Self::Vlan(VlanCmd::from_repr(c).ok_or(header)?)),
                (3, c) => {
                    Ok(Self::Announce(AnnounceCmd::from_repr(c).ok_or(header)?))
                }
                (4, c) => Ok(Self::Mq(MqCmd::from_repr(c).ok_or(header)?)),
                _ => Err(header),
            }
        }
    }

    #[derive(Clone, Copy, Debug, IntoBytes)]
    #[repr(u8)]
    pub enum Ack {
        Ok = 0,
        Err = 1,
    }

    #[derive(Clone, Copy, Debug, strum::FromRepr)]
    #[repr(u8)]
    pub enum RxCmd {
        Promisc = 0,
        AllMulticast = 1,
        AllUnicast = 2,
        NoMulticast = 3,
        NoUnicast = 4,
        NoBroadcast = 5,
    }

    /// The payload accompanying an [`RxCmd`].
    #[derive(Clone, Copy, Debug, Default, FromBytes)]
    #[repr(C)]
    pub struct Rx {
        /// Whether the flag signalled by the command should be enabled (`1`)
        /// or disabled (`0`).
        pub set: u8,
    }

    #[derive(Clone, Copy, Debug, strum::FromRepr)]
    #[repr(u8)]
    pub enum MacCmd {
        TableSet = 0,
        AddrSet = 1,
    }

    #[derive(Clone, Copy, Debug, Default, FromBytes)]
    #[repr(C)]
    pub struct Mq {
        pub npairs: u16,
    }

    #[derive(Clone, Copy, Debug, strum::FromRepr)]
    #[repr(u8)]
    pub enum MqCmd {
        SetPairs = 0,
        RssConfig = 1,
        HashConfig = 2,
    }

    impl TryFrom<u8> for MqCmd {
        type Error = u8;
        fn try_from(value: u8) -> Result<MqCmd, Self::Error> {
            match value {
                0 => Ok(Self::SetPairs),
                v => Err(v),
            }
        }
    }

    #[derive(Clone, Copy, Debug, strum::FromRepr)]
    #[repr(u8)]
    pub enum VlanCmd {
        FilterAdd = 0,
        FilterDelete = 1,
    }
    #[derive(Clone, Copy, Debug, strum::FromRepr)]
    #[repr(u8)]
    pub enum AnnounceCmd {
        Ack = 0,
    }

    /// Read a MAC filter table from a control queue.
    ///
    /// These are encoded as a `u32` followed by that number of
    /// 6-byte MAC addresses.
    pub(super) fn read_mac_list(
        chain: &mut super::Chain,
        mem: &super::MemCtx,
    ) -> Result<Box<[MacAddr]>, ()> {
        let mut entry_count = 0u32;
        if !chain.read(&mut entry_count, mem) {
            return Err(());
        }

        chain
            .read_many_owned(mem, entry_count as usize)
            .map_err(|_| ())
            .map(Vec::into_boxed_slice)
    }
}

pub mod migrate {
    use super::*;
    use crate::migrate::*;

    #[derive(Copy, Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
    pub enum PromiscMode {
        None,
        AllMulti,
        All,
        AllVlan,
    }

    impl From<PromiscLevel> for PromiscMode {
        fn from(value: PromiscLevel) -> Self {
            match value {
                PromiscLevel::None => Self::None,
                PromiscLevel::AllMulti => Self::AllMulti,
                PromiscLevel::All => Self::All,
                #[cfg(feature = "falcon")]
                PromiscLevel::AllVlan => Self::AllVlan,
            }
        }
    }

    // Note: `multicast_table_managed` comes after V1 has already shipped.
    // A payload from older sources omits this field, defaulting to false and
    // retaining all-multicast mode. Pre-MAC-filter targets will just ignore it.
    #[derive(Deserialize, Serialize)]
    pub struct VionaStateV1 {
        pub promisc: PromiscMode,
        pub filter: u8,
        pub unicast_mac_filters: Vec<MacAddr>,
        pub multicast_mac_filters: Vec<MacAddr>,
        /// Whether the source had accepted a `MAC_TABLE_SET` since the time
        /// of its last reset.
        #[serde(default)]
        pub multicast_table_managed: bool,
    }

    impl From<&Inner> for VionaStateV1 {
        fn from(state: &Inner) -> Self {
            // A pre-MAC-filter target version restores `promisc`, but does not
            // install the migrated multicast table.
            //
            // We export all-multicast mode when delivery currently depends
            // on that table, so the migration does not drop multicast.
            let promisc = if state.mac_filters_installed
                && state.promisc == PromiscLevel::None
            {
                PromiscLevel::AllMulti
            } else {
                state.promisc
            };

            Self {
                promisc: promisc.into(),
                filter: state.filter.bits(),
                unicast_mac_filters: state.unicast_mac_filters.to_vec(),
                multicast_mac_filters: state
                    .multicast_mac_filters
                    .iter()
                    .copied()
                    .map(MacAddr::from)
                    .collect(),
                multicast_table_managed: state.mac_table_set,
            }
        }
    }

    impl Schema<'_> for VionaStateV1 {
        fn id() -> SchemaId {
            ("pci-virtio-viona", 1)
        }
    }
}

/// Viona's in-kernel emulation of the device VirtQueues is performed in what
/// are calls "vrings". Since the userspace portion of the Viona emulation is
/// tasked with keeping the vring state in sync with the VirtQueue it
/// represents, we must track its perceived state.
#[derive(Copy, Clone, Default, Eq, PartialEq, Debug)]
enum VRingState {
    /// Initial state of the vring as it comes out of reset
    ///
    /// No guest-physical addresses, interrupt configuration, or avail/used
    /// indices are set on the vring.
    #[default]
    Init,

    /// Address(es) to valid VirtQueue data has been loaded into the vring but
    /// it has not been "kicked" to begin any processing.
    Ready,

    /// The vring has been "kicked" and it is proceeding to process TX/RX work
    /// as possible.
    Run,

    /// The vring has been issued a pause command to temporarily cease
    /// processing any work.  This is to allow the userspace emulation to gather
    /// a consistent snapshot of vring state.
    Paused,

    /// An error occurred while attempting to manipulate the vring.  This could
    /// be due to invalid configuration from the guest, or programmer error
    /// leading to unexpected device conditions.  If guest actions reset the
    /// vring state (by resetting the device, or reprogramming the VirtQueue),
    /// the vring can transition out of this error state.
    Error,

    /// An error occurred while attempting to reset the vring state.  This is
    /// unrecoverable and will assert a "failed" state on the VirtIO device as a
    /// whole.
    Fatal,
}

bitflags! {
    /// Packet receive filters requested by a virtio NIC driver.
    ///
    /// Each filter requested by the guest is a discrete value of
    /// [`control::RxCmd`], configured with a binary state (on/off). The device
    /// is responsible for determining which traffic should be allowed and
    /// filtered based on how these flags take priority over one another. We
    /// represent this as a bitfield for simplicity.
    #[derive(Copy, Clone, Debug)]
    struct FilterState: u8 {
        /// The driver has requested that we enter promiscuous mode, and filter
        /// no packets based on MAC address. This supersedes all other active
        /// flags.
        ///
        /// Requires [`VIRTIO_NET_F_CTRL_RX`].
        const PROMISCUOUS = 1 << 0;
        /// The driver has requested that we allow it to receive all multicast
        /// packets, regardless of filter table state.
        ///
        /// Requires [`VIRTIO_NET_F_CTRL_RX`].
        const ALL_MULTICAST = 1 << 1;
        /// The driver has requested that we allow it to receive all unicast
        /// packets, regardless of filter table state.
        ///
        /// Requires [`VIRTIO_NET_F_CTRL_RX_EXTRA`].
        const ALL_UNICAST = 1 << 2;
        /// The driver has requested that we drop all multicast packets, except
        /// from broadcast frames. This supersedes [`Self::ALL_MULTICAST`].
        ///
        /// Requires [`VIRTIO_NET_F_CTRL_RX_EXTRA`].
        const NO_MULTICAST = 1 << 3;
        /// The driver has requested that we drop all unicast packets, except
        /// from broadcast frames. This supersedes [`Self::ALL_UNICAST`].
        ///
        /// Requires [`VIRTIO_NET_F_CTRL_RX_EXTRA`].
        const NO_UNICAST = 1 << 4;
        /// The driver has requested that we drop all broadcast packets.
        /// This supersedes [`Self::ALL_MULTICAST`].
        ///
        /// Requires [`VIRTIO_NET_F_CTRL_RX_EXTRA`].
        const NO_BROADCAST = 1 << 5;

        /// All commands exposed by [`VIRTIO_NET_F_CTRL_RX`].
        const RX_CMDS = Self::PROMISCUOUS.bits() | Self::ALL_MULTICAST.bits();

        /// All commands exposed by [`VIRTIO_NET_F_CTRL_RX_EXTRA`].
        const RX_EXTRA_CMDS = Self::ALL_UNICAST.bits()
            | Self::NO_MULTICAST.bits()
            | Self::NO_UNICAST.bits()
            | Self::NO_BROADCAST.bits();
    }
}

/// The kernel-side Rx configuration required to meet a guest's filter state.
#[derive(Copy, Clone, Eq, PartialEq)]
struct RxConfig {
    promisc: PromiscLevel,
    /// Whether reconciliation should install the guest's non-empty multicast
    /// table.
    ///
    /// Under the kernel's [overlap contract], viona drops classified multicast
    /// copies of packets already delivered through the promiscuous-multicast
    /// callback while `VIONA_PROMISC_MULTI` is active.
    ///
    /// [overlap contract]: https://github.com/oxidecomputer/illumos-gate/blob/5ffff4b86e486e1f9d7860be1368386699a7829a/usr/src/uts/intel/sys/viona_io.h#L197-L202
    install_mcast_filters: bool,
}

struct Inner {
    poller: Option<PollerHdl>,
    iop_state: Option<NonZeroU16>,
    notify_mmio_addr: Option<u64>,
    vring_state: Vec<VRingState>,

    promisc: PromiscLevel,
    filter: FilterState,
    unicast_mac_filters: Box<[MacAddr]>,
    multicast_mac_filters: Box<[MulticastMacAddr]>,
    /// Whether a multicast filter table is installed on the in-kernel MAC
    /// client via `VNA_IOC_SET_MAC_FILTERS`.
    mac_filters_installed: bool,
    rx_config_failed: bool,
    /// Whether the guest's MAC table management is active.
    ///
    /// An accepted table, including an empty one, allows
    /// [`PciVirtioViona::rx_config`] to release the all-multicast
    /// lower bound.
    ///
    /// This bound exists for illumos vioif, which negotiates
    /// `VIRTIO_NET_F_CTRL_RX` but never programs a multicast
    /// table: <https://www.illumos.org/issues/18280>.
    mac_table_set: bool,
}
impl Inner {
    fn new(max_queues: usize, promisc: PromiscLevel) -> Self {
        let vring_state = vec![Default::default(); max_queues];
        let poller = None;
        let iop_state = None;
        let notify_mmio_addr = None;
        Self {
            poller,
            iop_state,
            notify_mmio_addr,
            vring_state,
            promisc,
            filter: FilterState::empty(),
            unicast_mac_filters: Box::new([]),
            multicast_mac_filters: Box::new([]),
            mac_filters_installed: false,
            rx_config_failed: false,
            mac_table_set: false,
        }
    }

    /// Get the `VRingState` for a given VirtQueue
    fn for_vq(&mut self, vq: &VirtQueue) -> &mut VRingState {
        let id = vq.id as usize;
        assert!(id < self.vring_state.len());
        &mut self.vring_state[id]
    }
}

/// Configuration parmaeters for the underlying viona device
#[derive(Copy, Clone)]
pub struct DeviceParams {
    /// When transmitting packets, should viona (allocate and) copy the entire
    /// contents of the packet, rather than "loaning" the guest memory beyond
    /// the packet headers?
    ///
    /// There is a performance cost to copying the full packet, but it avoids
    /// certain issues pertaining to looped-back viona packets being delivered
    /// to native zones on the machine.
    ///
    /// This parameter requires [viona_api::ApiVersion::V3] or greater. This is
    /// before Propolis' minimum viona API version and can always be set.
    pub copy_data: bool,

    /// Byte count for padding added to the head of transmitted packets.  This
    /// padding can be used by subsequent operations in the transmission chain,
    /// such as encapsulation, which would otherwise need to re-allocate for the
    /// larger header.
    ///
    /// This parameter requires [viona_api::ApiVersion::V3] or greater. This is
    /// before Propolis' minimum viona API version and can always be set.
    pub header_pad: u16,
}
impl DeviceParams {
    #[cfg(target_os = "illumos")]
    fn set(&self, hdl: &VionaHdl) -> io::Result<()> {
        // Set parameters assuming an ApiVersion::V3 device
        let mut params = viona_api::NvList::new();
        params.add(c"tx_copy_data", self.copy_data);
        params.add(c"tx_header_pad", self.header_pad);
        if let Err(e) = hdl.0.set_parameters(&mut params) {
            match e {
                viona_api::ParamError::Io(io) => Err(io),
                viona_api::ParamError::Detailed(_) => Err(Error::new(
                    ErrorKind::InvalidInput,
                    "unsupported viona parameters",
                )),
            }
        } else {
            Ok(())
        }
    }

    #[cfg(not(target_os = "illumos"))]
    fn set(&self, _hdl: &VionaHdl) -> io::Result<()> {
        panic!("viona and libnvpair not present on non-illumos")
    }
}
impl Default for DeviceParams {
    fn default() -> Self {
        // Viona (as of V3) allocs/copies entire packet by default, with no
        // padding added to the header.
        Self { copy_data: true, header_pad: 0 }
    }
}

/// Relaxed levels of packet filtering offered by viona.
#[derive(
    Copy,
    Clone,
    Debug,
    Default,
    Eq,
    PartialEq,
    Ord,
    PartialOrd,
    Serialize,
    Deserialize,
)]
pub enum PromiscLevel {
    /// The device should receive only packets for its installed MAC
    /// filters.
    ///
    /// Today this allows solely `PciVirtioViona::mac_addr`.
    #[default]
    None,
    /// The device should receive all multicast traffic in addition
    /// to its registered unicast filters.
    AllMulti,
    /// The device should receive all packets.
    All,
    #[cfg(feature = "falcon")]
    /// The device should receive all packets, including VLAN tags.
    ///
    /// This suggests a bug in viona: when `VIRTIO_NET_F_CTRL_VLAN` is
    /// not negotiated, the device _should_ accept all VLAN-tagged frames.
    ///
    /// This mechanism is only present on the branch
    /// oxidecomputer/illumos-gate/viona_vlans. If the OS supports this,
    /// then we must remain in this mode if set to ensure hosts receive
    /// the correct traffic.
    AllVlan,
}

impl From<PromiscLevel> for usize {
    fn from(value: PromiscLevel) -> Self {
        (match value {
            PromiscLevel::None => viona_api::VIONA_PROMISC_NONE,
            PromiscLevel::AllMulti => viona_api::VIONA_PROMISC_MULTI,
            PromiscLevel::All => viona_api::VIONA_PROMISC_ALL,
            #[cfg(feature = "falcon")]
            PromiscLevel::AllVlan => viona_api::VIONA_PROMISC_ALL_VLAN,
        }) as usize
    }
}

/// A MAC address.
#[derive(
    Copy,
    Clone,
    Debug,
    Default,
    Eq,
    PartialEq,
    Ord,
    PartialOrd,
    FromBytes,
    IntoBytes,
    Immutable,
    Serialize,
    Deserialize,
)]
pub struct MacAddr([u8; ETHERADDRL]);

impl From<[u8; ETHERADDRL]> for MacAddr {
    fn from(value: [u8; ETHERADDRL]) -> Self {
        Self(value)
    }
}

impl MacAddr {
    pub const fn is_unicast(&self) -> bool {
        (self.0[0] & 0b1) == 0
    }
}

/// A [`MacAddr`] with the IEEE 802.3 group bit set for multicast filter
/// table use.
///
/// Broadcast also qualifies here and is admitted, but the kernel drops
/// these entries during compaction anyway.
#[repr(transparent)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, IntoBytes, Immutable)]
struct MulticastMacAddr(MacAddr);

impl TryFrom<MacAddr> for MulticastMacAddr {
    type Error = MacAddr;

    fn try_from(value: MacAddr) -> Result<Self, Self::Error> {
        if value.is_unicast() {
            Err(value)
        } else {
            Ok(Self(value))
        }
    }
}

impl From<MulticastMacAddr> for MacAddr {
    fn from(value: MulticastMacAddr) -> Self {
        value.0
    }
}

/// Represents a connection to the kernel's Viona (VirtIO Network Adapter)
/// driver.
pub struct PciVirtioViona {
    virtio_state: PciVirtioState,
    pci_state: pci::DeviceState,
    indicator: lifecycle::Indicator,

    dev_features: u64,
    mac_addr: MacAddr,
    mtu: Option<u16>,
    hdl: VionaHdl,
    inner: Mutex<Inner>,
}

impl PciVirtioViona {
    pub fn new(
        vnic_name: &str,
        vm: &VmmHdl,
        viona_params: Option<DeviceParams>,
    ) -> io::Result<Arc<PciVirtioViona>> {
        Self::new_with_queue_sizes(
            vnic_name,
            RX_QUEUE_SIZE,
            TX_QUEUE_SIZE,
            CTL_QUEUE_SIZE,
            vm,
            viona_params,
        )
    }

    pub fn new_with_queue_sizes(
        vnic_name: &str,
        rx_queue_size: VqSize,
        tx_queue_size: VqSize,
        ctl_queue_size: VqSize,
        vm: &VmmHdl,
        viona_params: Option<DeviceParams>,
    ) -> io::Result<Arc<PciVirtioViona>> {
        let dlhdl = dladm::Handle::new()?;
        let info = dlhdl.query_link(vnic_name)?;
        let hdl = VionaHdl::new(info.link_id, vm.fd())?;

        // Viona is configured for all-multicast delivery by default.
        //
        // Builds with the `falcon` feature request all-VLAN delivery when the
        // kernel supports it, since that mode is required to receive
        // VLAN-tagged traffic.
        #[cfg(not(feature = "falcon"))]
        let promisc_level = PromiscLevel::AllMulti;
        // Note: On Falcon builds, we configure the kernel handle before
        // constructing `PciVirtioViona`. The wrapper's `Inner` state is
        // initialized from the resulting `promisc_level`.
        #[cfg(feature = "falcon")]
        let promisc_level = match hdl.set_promisc(PromiscLevel::AllVlan) {
            Ok(()) => PromiscLevel::AllVlan,
            Err(e) if e.raw_os_error() == Some(libc::EINVAL) => {
                eprintln!(
                    "kernel does not support VIONA_PROMISC_ALL_VLAN on \
                     {vnic_name}: {e:?}"
                );
                PromiscLevel::AllMulti
            }
            Err(e) => return Err(e),
        };

        if let Some(vp) = viona_params {
            vp.set(&hdl)?;
        }

        // Do in-kernel configuration of device MTU
        if let Some(mtu) = info.mtu {
            if hdl.api_version().unwrap() >= viona_api::ApiVersion::V4 {
                hdl.set_mtu(mtu)?;
            } else if mtu != 1500 {
                // Squawk about MTUs not matching the default of 1500
                return Err(io::Error::new(
                    ErrorKind::Unsupported,
                    "viona device version is inadequate to set MTU",
                ));
            }
        }

        let queue_sizes = [rx_queue_size, tx_queue_size]
            .into_iter()
            .cycle()
            .take(max_num_queues())
            .chain([ctl_queue_size])
            .collect::<Vec<VqSize>>();
        // The vector is sized with the maximum number of rings/queues, but
        // until the driver negotiates multiqueue, we only use the first two
        // for the datapath. `queue_sizes` always contains at least three
        // elements -- the third will serve as the control queue if
        // multiqueue is not negotiated, even if it is a little large for that
        // purpose.
        let queues = VirtQueues::new_with_len(2, &queue_sizes);
        let nqueues = queues.max_capacity();
        hdl.set_pairs(1).unwrap();

        // Add one for config space.
        let msix_count = Some(1 + nqueues as u16);
        let (virtio_state, pci_state) = PciVirtioState::new(
            virtio::Mode::Transitional,
            queues,
            msix_count,
            virtio::DeviceId::Network,
            VIRTIO_NET_CFG_SIZE,
        );

        let dev_features = hdl.get_avail_features()?;
        let this = PciVirtioViona {
            virtio_state,
            pci_state,
            indicator: Default::default(),
            dev_features,
            mac_addr: info.mac_addr.into(),
            mtu: info.mtu,
            hdl,
            inner: Mutex::new(Inner::new(nqueues, promisc_level)),
        };
        let this = Arc::new(this);

        // Spawn the interrupt poller
        let mut inner = this.inner.lock().unwrap();
        inner.poller =
            Some(Poller::spawn(this.hdl.as_raw_fd(), Arc::downgrade(&this))?);
        drop(inner);

        Ok(this)
    }

    /// Get the minor instance number of the viona device.
    pub fn instance_id(&self) -> io::Result<u32> {
        self.hdl.instance_id()
    }

    fn process_interrupts(&self) {
        if let Some(mem) = self.pci_state.acc_mem.access() {
            self.hdl
                .intr_poll(self.virtio_state.queues.len(), |vq_idx| {
                    self.hdl.ring_intr_clear(vq_idx).unwrap();
                    let vq = self.virtio_state.queues.get(vq_idx).unwrap();
                    vq.send_intr(&mem);
                })
                .unwrap();
        }
    }

    fn ctl_queue_notify(&self, vq: &VirtQueue) {
        if let Some(mem) = self.pci_state.acc_mem.access() {
            while !vq.avail_is_empty(&mem) {
                let mut chain = Chain::with_capacity(4);
                let intrs_en = vq.disable_intr(&mem);
                while let Some((_idx, _len)) = vq.pop_avail(&mut chain, &mem) {
                    let res = match self.ctl_msg(&mut chain, &mem) {
                        Ok(_) => control::Ack::Ok,
                        Err(_) => control::Ack::Err,
                    } as u8;
                    chain.write(&res, &mem);
                    vq.push_used(&mut chain, &mem);
                }
                if intrs_en {
                    vq.enable_intr(&mem);
                }
            }
        }
    }

    fn ctl_msg(&self, chain: &mut Chain, mem: &MemCtx) -> Result<(), ()> {
        let mut header = control::Header::default();
        if !chain.read(&mut header, &mem) {
            return Err(());
        }
        probes::virtio_viona_cq_request!(|| (header.class, header.command));

        use control::Command;
        match Command::try_from(header).map_err(|_| ())? {
            Command::Rx(cmd) => self.ctl_rx(cmd, chain, mem),
            Command::Mac(cmd) => self.ctl_mac(cmd, chain, mem),
            // We do not yet advertise `VIRTIO_NET_F_CTRL_VLAN`.
            Command::Vlan(_) => Err(()),
            // We do not yet advertise `VIRTIO_NET_F_GUEST_ANNOUNCE`
            Command::Announce(_) => Err(()),
            Command::Mq(cmd) => self.ctl_mq(cmd, chain, mem),
        }
    }

    fn ctl_rx(
        &self,
        cmd: control::RxCmd,
        chain: &mut Chain,
        mem: &MemCtx,
    ) -> Result<(), ()> {
        use control::RxCmd;
        let filter = match cmd {
            RxCmd::Promisc => FilterState::PROMISCUOUS,
            RxCmd::AllMulticast => FilterState::ALL_MULTICAST,
            RxCmd::AllUnicast => FilterState::ALL_UNICAST,
            RxCmd::NoMulticast => FilterState::NO_MULTICAST,
            RxCmd::NoUnicast => FilterState::NO_UNICAST,
            RxCmd::NoBroadcast => FilterState::NO_BROADCAST,
        };

        let mut msg = control::Rx::default();
        if !chain.read(&mut msg, &mem) {
            return Err(());
        }
        let active = match msg.set {
            0 => false,
            1 => true,
            _ => return Err(()),
        };
        self.set_filter_state(filter, active)
    }

    fn ctl_mac(
        &self,
        cmd: control::MacCmd,
        chain: &mut Chain,
        mem: &MemCtx,
    ) -> Result<(), ()> {
        use control::MacCmd;
        match cmd {
            MacCmd::TableSet => {
                let unicast = control::read_mac_list(chain, mem)?;
                let multicast = control::read_mac_list(chain, mem)?;

                self.set_mac_filters(unicast, &multicast)
            }
            // We do not advertise `VIRTIO_NET_F_CTRL_MAC_ADDR`
            MacCmd::AddrSet => return Err(()),
        }
    }

    fn set_use_pairs(&self, requested: u16) -> Result<(), ()> {
        if requested < 1 || PROPOLIS_MAX_MQ_PAIRS < requested {
            return Err(());
        }
        let npairs = requested as usize;
        let nqueues = npairs * 2;
        if nqueues == self.virtio_state.queues.len() {
            return Ok(());
        }
        self.hdl.set_usepairs(requested).unwrap();
        self.virtio_state.queues.set_len(nqueues).expect("num queue pairs");
        Ok(())
    }

    fn ctl_mq(
        &self,
        cmd: control::MqCmd,
        chain: &mut Chain,
        mem: &MemCtx,
    ) -> Result<(), ()> {
        use control::MqCmd;
        match cmd {
            MqCmd::SetPairs => {
                let mut msg = control::Mq::default();
                if !chain.read(&mut msg, &mem) {
                    return Err(());
                }

                let npairs = msg.npairs;
                probes::virtio_viona_mq_set_use_pairs!(|| (
                    MqSetPairsCause::Commanded as u8,
                    npairs
                ));
                self.set_use_pairs(npairs)
            }
            MqCmd::RssConfig => Err(()),
            MqCmd::HashConfig => Err(()),
        }
    }

    fn net_cfg_read(&self, id: &NetReg, ro: &mut ReadOp) {
        match id {
            NetReg::Mac => ro.write_bytes(&self.mac_addr.0),
            NetReg::Status => {
                // Always report link up
                ro.write_u16(VIRTIO_NET_S_LINK_UP);
            }
            NetReg::MaxVqPairs => {
                ro.write_u16(PROPOLIS_MAX_MQ_PAIRS);
            }
            NetReg::Mtu => {
                // Guests should not be asking for this value unless
                // VIRTIO_NET_F_MTU has been set. However, we'd rather lie
                // (return zero) than unwrap and panic here.
                ro.write_u16(self.mtu.unwrap_or(0));
            }
            NetReg::Speed
            | NetReg::Duplex
            | NetReg::RssMaxKeySize
            | NetReg::RssMaxIndirectionTableLen
            | NetReg::SupportedHashTypes => {}
        }
    }

    /// Pause the associated virtqueues and sync any in-kernel state for them
    /// into the userspace representation.
    fn queues_sync(&self) {
        let mut inner = self.inner.lock().unwrap();
        for vq in self.virtio_state.queues.iter() {
            // If the queue is not alive, there's nothing to do here.
            if !vq.is_alive() {
                continue;
            }

            let rs = inner.for_vq(vq);
            match *rs {
                VRingState::Ready | VRingState::Run | VRingState::Paused => {
                    // A control queue has no in-kernel state to synchronize.
                    // If this is the case, we simply mark the ring paused
                    // and continue.
                    if vq.is_control() {
                        *rs = VRingState::Paused;
                        continue;
                    }

                    // Ensure the ring is paused for a consistent snapshot
                    if *rs != VRingState::Paused {
                        if self.hdl.ring_pause(vq).is_err() {
                            *rs = VRingState::Error;
                            continue;
                        }
                        *rs = VRingState::Paused;
                    }

                    if let Ok(live) = self.hdl.ring_get_state(vq) {
                        let base = vq.get_state();
                        assert_eq!(
                            live.mapping.desc_addr,
                            base.mapping.desc_addr
                        );
                        vq.set_state(&queue::Info {
                            used_idx: live.used_idx,
                            avail_idx: live.avail_idx,
                            ..base
                        });
                    } else {
                        *rs = VRingState::Error;
                    }
                }
                _ => {
                    // The vring is in a state where it is either redundant to
                    // sync the state (Init), or impossible (Error, Fatal)
                }
            }
        }
    }

    fn queues_restart(&self) -> Result<(), ()> {
        let mut inner = self.inner.lock().unwrap();
        let mut res = Ok(());
        for vq in self.virtio_state.queues.iter() {
            let rs = inner.for_vq(vq);

            // The existing state machine for vrings in Viona does not allow for
            // a Paused -> Running transition, requiring instead that the vring
            // be reset and reloaded with state in order to proceed again.
            if self.hdl.ring_reset(vq).is_err() {
                *rs = VRingState::Fatal;
                res = Err(());
                // Although this fatal vring state means the device itself will
                // require a reset (which itself is unlikely to work), we
                // continue attempting to reset/restart the other VQs.
                continue;
            }

            *rs = VRingState::Init;
            if vq.is_mapped() {
                if self.hdl.ring_set_state(vq.as_ref()).is_err() {
                    *rs = VRingState::Error;
                    continue;
                }

                if let Some(intr_cfg) = vq.read_intr() {
                    if self.hdl.ring_cfg_msi(vq, Some(intr_cfg)).is_err() {
                        *rs = VRingState::Error;
                        continue;
                    }
                }
                *rs = VRingState::Ready;

                if vq.is_alive() {
                    // If the ring was already running, kick it.
                    if self.hdl.ring_kick(vq).is_err() {
                        *rs = VRingState::Error;
                        continue;
                    }
                    *rs = VRingState::Run;
                }
            }
        }
        res
    }

    /// Make sure all in-kernel virtqueue processing is stopped
    fn queues_kill(&self) {
        self.virtio_state.reset_queues(self);
    }

    fn poller_start(&self) {
        let mut inner = self.inner.lock().unwrap();
        let poller = inner.poller.as_mut().expect("poller should be spawned");
        let wait_state = poller.state.clone();
        let _ = poller.sender.send(TargetState::Run);
        drop(inner);
        // wait_running() will wait on a condition variable, but the signaller
        // of that condition variable is the Poller task that we've also spawned
        // on this runtime. `block_in_place` to avoid blocking this runtime
        // thread and help make sure the Poller we've asked to start actually
        // can.
        tokio::task::block_in_place(|| wait_state.wait_running());
    }
    fn poller_stop(&self, should_exit: bool) {
        let mut inner = self.inner.lock().unwrap();
        let wait_state = if should_exit {
            let poller = inner.poller.take().expect("poller should be spawned");
            let _ = poller.sender.send(TargetState::Exit);
            poller.state
        } else {
            let poller =
                inner.poller.as_mut().expect("poller should be spawned");
            let _ = poller.sender.send(TargetState::Pause);
            poller.state.clone()
        };
        drop(inner);
        // Same general problem as `wait_running` in `poller_start` above.
        tokio::task::block_in_place(|| wait_state.wait_stopped());
    }

    // Transition the emulation to a "running" state, either at initial start-up
    // or resumption from a "paused" state.
    fn run(&self) {
        self.poller_start();
        if self.queues_restart().is_err() {
            self.virtio_state.set_needs_reset(self);
            self.notify_port_update(None);
            self.notify_mmio_addr_update(None);
        } else {
            // If all is well with the queue restart, attempt to wire up the
            // notification ioport again.
            let state = self.inner.lock().unwrap();
            let _ = self.hdl.set_notify_io_port(state.iop_state);
            let _ = self.hdl.set_notify_mmio_addr(state.notify_mmio_addr);
        }
    }

    /// Set or unset unicast/multicast class-wide filters on behalf of a driver.
    fn set_filter_state(
        &self,
        filter: FilterState,
        active: bool,
    ) -> Result<(), ()> {
        if (self.virtio_state.negotiated_features() & VIRTIO_NET_F_CTRL_RX) == 0
            && filter.intersects(FilterState::RX_CMDS)
        {
            return Err(());
        }

        if filter.intersects(FilterState::RX_EXTRA_CMDS) {
            // We cannot express any of the extra filters within viona yet.
            return Err(());
        }

        let mut state = self.inner.lock().unwrap();
        if state.rx_config_failed {
            return Err(());
        }
        state.filter.set(filter, active);
        self.reconcile_rx_and_release(state, RxReconcileCause::FilterChange)
            .map_err(|_| ())
    }

    /// Replace the requested set of explicit MAC address filters on a device
    /// with a new table provided by the driver.
    fn set_mac_filters(
        &self,
        unicast: Box<[MacAddr]>,
        multicast: &[MacAddr],
    ) -> Result<(), ()> {
        if (self.virtio_state.negotiated_features() & VIRTIO_NET_F_CTRL_RX) == 0
        {
            return Err(());
        }

        if unicast.iter().any(|v| !v.is_unicast()) {
            return Err(());
        }
        let multicast = multicast
            .iter()
            .map(|&v| MulticastMacAddr::try_from(v))
            .collect::<Result<Box<[_]>, _>>()
            .map_err(|_| ())?;

        let mut state = self.inner.lock().unwrap();
        if state.rx_config_failed {
            return Err(());
        }
        let table_changed = multicast != state.multicast_mac_filters;
        state.unicast_mac_filters = unicast;
        state.multicast_mac_filters = multicast;
        state.mac_table_set = true;
        self.reconcile_rx_and_release(
            state,
            if table_changed {
                RxReconcileCause::TableReplaced
            } else {
                RxReconcileCause::FilterChange
            },
        )
        .map_err(|_| ())
    }

    /// Update the promisc level of the device.
    fn set_promisc(
        &self,
        level: PromiscLevel,
        state: &mut Inner,
    ) -> Result<(), RxConfigError> {
        match self.hdl.set_promisc(level) {
            Ok(_) => {
                state.promisc = level;
                Ok(())
            }
            Err(e) => {
                probes::virtio_viona_promisc_err!(|| (
                    usize::from(level) as u8,
                    usize::from(state.promisc) as u8,
                    e.raw_os_error().unwrap_or(0),
                ));
                Err(RxConfigError::SetPromisc {
                    previous: state.promisc,
                    requested: level,
                    source: e,
                })
            }
        }
    }

    /// Compute the Rx configuration required to fulfill the driver's MAC
    /// filters setup and explicit filter mode.
    ///
    /// On default builds, this is computed from the guest's filter state and
    /// device capabilities.
    ///
    /// With `falcon` enabled, it also preserves any host-required promiscuity.
    fn rx_config(&self, state: &Inner) -> RxConfig {
        // The VLAN tag workaround, if requested, always wins and cannot
        // be downgraded.
        #[cfg(feature = "falcon")]
        if state.promisc == PromiscLevel::AllVlan {
            // This host-level pin supersets every promiscuity level the guest
            // can request.
            return RxConfig {
                promisc: PromiscLevel::AllVlan,
                install_mcast_filters: false,
            };
        }

        let should_install_mcast = !state.multicast_mac_filters.is_empty();
        let need_mcast_promisc =
            state.filter.contains(FilterState::ALL_MULTICAST);

        // Avoid enabling promiscuous mode for drivers that request only their
        // own MAC address. Most guests *should not pass any unicast
        // addresses*, as the config-space MAC is assumed to be included by
        // default. `.all()` will return `true` for such an empty list. This is
        // defensive handling for that case.
        let filter_is_self =
            state.unicast_mac_filters.iter().all(|mac| mac == &self.mac_addr);
        let need_promisc =
            state.filter.contains(FilterState::PROMISCUOUS) || !filter_is_self;

        // Until guest MAC table management has been established, we
        // keep all-multicast delivery enabled.
        //
        // illumos vioif negotiates CTRL_RX but never sends a table.
        // Narrowing it to classified delivery after a promiscuity cycle could
        // drop its multicast traffic: <https://www.illumos.org/issues/18280>.
        //
        // Note: Unrequested traffic is permitted by the VirtIO spec.
        let promisc = if need_promisc {
            PromiscLevel::All
        } else if need_mcast_promisc || !state.mac_table_set {
            PromiscLevel::AllMulti
        } else {
            PromiscLevel::None
        };

        RxConfig { promisc, install_mcast_filters: should_install_mcast }
    }

    /// Reconcile the in-kernel Rx configuration with the guest's filter
    /// state, as computed by [`Self::rx_config`].
    ///
    /// This installs a filter table before lowering promiscuity, raising
    /// promiscuity before clearing a table. Both orders overdeliver in the
    /// window between the two ioctls.
    ///
    /// The device SHOULD drop unmatched packets, but unwanted packets may still
    /// arrive (VirtIO 1.2, 5.1.6.5.1 and 5.1.6.5.2.1). A packet lost during the
    /// transition cannot be recovered.
    ///
    /// A failure leaves the kernel configuration in an undefined state.
    /// Callers must set `NEEDS_RESET` on the device rather than try
    /// to repair it.
    fn apply_rx_config(
        &self,
        state: &mut Inner,
        cause: RxReconcileCause,
    ) -> Result<(), RxConfigError> {
        let reinit = matches!(cause, RxReconcileCause::Reinitialize);
        if state.rx_config_failed && !reinit {
            return Err(RxConfigError::NeedsReset);
        }

        match self.try_apply_rx_config(state, cause) {
            Ok(()) => {
                if reinit {
                    state.rx_config_failed = false;
                }
                Ok(())
            }
            Err(primary) => {
                state.rx_config_failed = true;
                let fallback =
                    match (state.promisc, self.rx_config(state).promisc) {
                        #[cfg(feature = "falcon")]
                        (PromiscLevel::AllVlan, _)
                        | (_, PromiscLevel::AllVlan) => PromiscLevel::AllVlan,
                        (PromiscLevel::All, _) | (_, PromiscLevel::All) => {
                            PromiscLevel::All
                        }
                        _ => PromiscLevel::AllMulti,
                    };
                match self.set_promisc(fallback, state) {
                    Ok(()) => Err(primary),
                    Err(fallback_err) => Err(RxConfigError::FallbackPromisc {
                        primary: Box::new(primary),
                        fallback: Box::new(fallback_err),
                    }),
                }
            }
        }
    }

    fn try_apply_rx_config(
        &self,
        state: &mut Inner,
        cause: RxReconcileCause,
    ) -> Result<(), RxConfigError> {
        let RxConfig {
            promisc: mut effective_promisc,
            install_mcast_filters: wanted,
        } = self.rx_config(state);
        if state.rx_config_failed
            && matches!(cause, RxReconcileCause::Reinitialize)
        {
            let fallback = match effective_promisc {
                PromiscLevel::None => PromiscLevel::AllMulti,
                level => level,
            };
            self.set_promisc(fallback, state)?;
        }

        let mut action = McastTableAction::derive(
            wanted,
            state.mac_filters_installed,
            &cause,
        );

        if matches!(action, McastTableAction::Install) {
            match self.hdl.set_mac_filters(&state.multicast_mac_filters) {
                Ok(()) => state.mac_filters_installed = true,
                // The kernel rejected the count with the installed filters
                // untouched, reporting its capacity back through
                // vmf_nmcast.
                //
                // The guest's request stays in its state.
                Err(MacFilterError::Count { capacity }) => {
                    probes::virtio_viona_mac_filters_overflow!(|| (
                        state.multicast_mac_filters.len() as u32,
                        capacity,
                    ));
                    if effective_promisc == PromiscLevel::None {
                        effective_promisc = PromiscLevel::AllMulti;
                    }
                    action = McastTableAction::derive(
                        false,
                        state.mac_filters_installed,
                        &cause,
                    );
                }
                Err(e) => return Err(RxConfigError::InstallMacFilters(e)),
            }
        }

        // No table is required when it is empty, over capacity, or when host
        // promiscuity takes precedence. `Reinitialize` also clears the table
        // in order to re-establish known kernel state, even if local state has
        // no multicast table installed.
        if matches!(action, McastTableAction::Clear) {
            let promisc_before_clear = (effective_promisc > state.promisc)
                .then_some(effective_promisc);
            if let Some(level) = promisc_before_clear {
                self.set_promisc(level, state)?;
            }
            self.hdl
                .set_mac_filters(&[])
                .map_err(RxConfigError::ClearMacFilters)?;
            state.mac_filters_installed = false;
        }

        self.set_promisc(effective_promisc, state)?;
        Ok(())
    }

    /// Apply the Rx config while holding the `Inner` guard, and then
    /// release it before setting `NEEDS_RESET`.
    ///
    /// Feature writes can arrive while the PCI layer holds the VirtIO state
    /// lock; setting `NEEDS_RESET` reacquires that lock. Releasing
    /// the `Inner` guard avoids reversing lock order.
    fn reconcile_rx_and_release(
        &self,
        mut state: MutexGuard<Inner>,
        cause: RxReconcileCause,
    ) -> Result<(), RxConfigError> {
        let outcome = self.apply_rx_config(&mut state, cause);
        drop(state);
        if outcome.is_err() {
            self.virtio_state.set_needs_reset(self);
        }
        outcome
    }

    /// Return the guest-configurable Rx state to its post-construction
    /// values and apply reconciliation.
    fn clear_guest_rx_state(
        &self,
        state: &mut Inner,
    ) -> Result<(), RxConfigError> {
        state.filter = FilterState::empty();
        state.unicast_mac_filters = Box::new([]);
        state.multicast_mac_filters = Box::new([]);
        state.mac_table_set = false;
        self.apply_rx_config(state, RxReconcileCause::Reinitialize)
    }
}
impl VirtioDevice for PciVirtioViona {
    fn rw_dev_config(&self, mut rwo: RWOp) {
        NET_DEV_REGS.process(&mut rwo, |id, rwo| match rwo {
            RWOp::Read(ro) => self.net_cfg_read(id, ro),
            RWOp::Write(_) => {
                // Ignore writes.
                //
                // Technically while we are in either `Mode::Transitional`
                // or `Mode::Legacy` the driver may write to `NetReg::Mac`
                // in lieu of sending a `MacCmd::AddrSet`. We don't support
                // changing the MAC address today.
            }
        });
    }
    fn mode(&self) -> virtio::Mode {
        self.virtio_state.mode()
    }

    fn features(&self) -> u64 {
        let mut feat = VIRTIO_NET_F_MAC
            | VIRTIO_NET_F_STATUS
            | VIRTIO_NET_F_CTRL_VQ
            | VIRTIO_NET_F_CTRL_RX
            | VIRTIO_NET_F_MQ;
        // We drop the "VIRTIO_NET_F_MTU" flag from feat if we are unable to
        // query it. This can happen when executing within a non-global Zone.
        //
        // Context: https://www.illumos.org/issues/13992
        if self.mtu.is_some() {
            feat |= VIRTIO_NET_F_MTU;
        }
        feat |= self.dev_features;

        feat
    }

    fn set_features(&self, feat: u64) -> Result<(), ()> {
        self.hdl.set_features(feat).map_err(|_| ())?;

        // Clear guest Rx state after applying the new feature set.
        //
        // Legacy devices can also update features without passing through the
        // modern (device) FEATURES_OK transition.
        {
            let mut state = self.inner.lock().unwrap();
            self.clear_guest_rx_state(&mut state).map_err(|_| ())?;
        }

        // Any remaining setup is for control-queue based features.
        let control_queue = if (feat & VIRTIO_NET_F_CTRL_VQ) == 0 {
            None
        } else {
            if self.virtio_state.queues.max_capacity() < 3 {
                // Since we're advertising control queue support, we need
                // one Rx, Tx, and CtlQ at the minimum.
                return Err(());
            }

            let ctl_q_idx = if (feat & VIRTIO_NET_F_MQ) != 0 {
                self.hdl.set_pairs(PROPOLIS_MAX_MQ_PAIRS).map_err(|_| ())?;
                probes::virtio_viona_mq_set_use_pairs!(|| (
                    MqSetPairsCause::MqEnabled as u8,
                    PROPOLIS_MAX_MQ_PAIRS
                ));
                self.set_use_pairs(PROPOLIS_MAX_MQ_PAIRS)?;
                self.virtio_state.queues.max_capacity() - 1
            } else {
                VIRTIO_NO_MQ_CTRL_Q_INDEX
            };

            Some(ctl_q_idx.try_into().expect("queue index must be a valid u16"))
        };

        self.virtio_state.queues.set_ctl_queues(control_queue.as_slice())
    }

    fn queue_notify(&self, vq: &VirtQueue) {
        if vq.is_control() {
            self.ctl_queue_notify(vq);
            return;
        }
        let mut inner = self.inner.lock().unwrap();
        let ring_state = inner.for_vq(vq);
        match ring_state {
            VRingState::Ready | VRingState::Run => {
                if self.hdl.ring_kick(vq).is_err() {
                    *ring_state = VRingState::Error;
                } else {
                    *ring_state = VRingState::Run;
                }
            }
            _ => {}
        }
    }
    fn queue_change(&self, vq: &VirtQueue, change: VqChange) -> Result<(), ()> {
        let mut inner = self.inner.lock().unwrap();
        let rs = inner.for_vq(vq);

        match change {
            VqChange::Reset => {
                if self.hdl.ring_reset(vq).is_err() {
                    *rs = VRingState::Fatal;
                    return Err(());
                }
                *rs = VRingState::Init;
            }
            VqChange::Address => {
                match *rs {
                    VRingState::Init => {}
                    VRingState::Ready
                    | VRingState::Run
                    | VRingState::Paused
                    | VRingState::Error => {
                        // Reset any vring not already in such a state
                        if self.hdl.ring_reset(vq).is_err() {
                            *rs = VRingState::Fatal;
                            return Err(());
                        }
                        *rs = VRingState::Init;
                    }
                    VRingState::Fatal => {
                        // No sense in trying anything further on a doomed vring
                        return Err(());
                    }
                }
                if !vq.is_mapped() {
                    return Ok(());
                }

                if !vq.is_control() && self.hdl.ring_init(vq).is_err() {
                    // Bad virtqueue configuration is not fatal.  While the
                    // vring will not transition to running, we will be content
                    // to wait for the guest to later provide a valid config.
                    *rs = VRingState::Error;
                    return Ok(());
                }

                if let Some(intr_cfg) = vq.read_intr() {
                    if self.hdl.ring_cfg_msi(vq, Some(intr_cfg)).is_err() {
                        *rs = VRingState::Error;
                    }
                }
                *rs = VRingState::Ready;
            }
            VqChange::IntrCfg => {
                if *rs != VRingState::Fatal {
                    let intr = vq.read_intr();
                    if self.hdl.ring_cfg_msi(vq, intr).is_err() {
                        *rs = VRingState::Error;
                    }
                }
            }
        }
        Ok(())
    }
}
impl Lifecycle for PciVirtioViona {
    fn type_name(&self) -> &'static str {
        "pci-virtio-viona"
    }
    fn reset(&self) {
        self.virtio_state.reset(self);
        probes::virtio_viona_mq_set_use_pairs!(|| (
            MqSetPairsCause::Reset as u8,
            1
        ));
        self.set_use_pairs(1).expect("can set viona back to one queue pair");
        self.hdl.set_pairs(1).expect("can set viona back to one queue pair");
        self.virtio_state.queues.reset_peak();

        // Rx state resets through the same path as feature renegotiation.
        let mut state = self.inner.lock().unwrap();
        let outcome = self.clear_guest_rx_state(&mut state);
        drop(state);
        if outcome.is_err() {
            self.virtio_state.set_needs_reset(self);
        }
    }
    fn start(&self) -> anyhow::Result<()> {
        self.run();
        self.indicator.start();
        Ok(())
    }
    fn pause(&self) {
        self.poller_stop(false);
        self.queues_sync();

        // In case the device is being paused because of a pending instance
        // reinitialization (as part of a reboot/reset), the notification ioport
        // binding must be torn down.  Bhyve will emit failure of an attempted
        // reinitialization operation if any ioport hooks persist at that time.
        let _ = self.hdl.set_notify_io_port(None);
        let _ = self.hdl.set_notify_mmio_addr(None);

        self.indicator.pause();
    }
    fn resume(&self) {
        self.run();
        self.indicator.resume();
    }
    fn halt(&self) {
        self.poller_stop(true);
        // Destroy any in-kernel state to prevent it from impeding instance
        // destruction.
        self.queues_kill();
        let _ = self.hdl.delete();
        self.indicator.halt();
    }
    fn migrate(&self) -> Migrator<'_> {
        Migrator::Multi(self)
    }
}

impl PciVirtio for PciVirtioViona {
    fn virtio_state(&self) -> &PciVirtioState {
        &self.virtio_state
    }
    fn pci_state(&self) -> &pci::DeviceState {
        &self.pci_state
    }
    // The notification addresses (both port and MMIO) for the device can change
    // due to guest action, or other administrative tasks within propolis.
    fn notify_port_update(&self, port: Option<NonZeroU16>) {
        let mut state = self.inner.lock().unwrap();
        state.iop_state = port;
        // We want to update the in-kernel IO port hook when the address is
        // updated due to guest action; that is, when the device emulation is
        // actually running.
        if self.indicator.state() == IndicatedState::Run {
            let _ = self.hdl.set_notify_io_port(port);
        }
    }
    fn notify_mmio_addr_update(&self, addr: Option<u64>) {
        let mut state = self.inner.lock().unwrap();
        state.notify_mmio_addr = addr;
        // Only update the io-kernel address hook when changed by guest action,
        // similarly to the port IO case above.
        if self.indicator.state() == IndicatedState::Run {
            let _ = self.hdl.set_notify_mmio_addr(addr);
        }
    }
}

impl MigrateMulti for PciVirtioViona {
    fn export(
        &self,
        output: &mut PayloadOutputs,
        ctx: &MigrateCtx,
    ) -> Result<(), MigrateStateError> {
        <dyn PciVirtio>::export(self, output, ctx)?;

        let viona_state = {
            let state = self.inner.lock().unwrap();
            migrate::VionaStateV1::from(&*state)
        };

        output.push(viona_state.into())
    }

    fn import(
        &self,
        offer: &mut PayloadOffers,
        ctx: &MigrateCtx,
    ) -> Result<(), MigrateStateError> {
        <dyn PciVirtio>::import(self, offer, ctx)?;

        let feat = self.virtio_state.negotiated_features();
        self.hdl.set_features(feat).map_err(|e| {
            MigrateStateError::ImportFailed(format!(
                "error while setting viona features ({feat:x}): {e:?}"
            ))
        })?;

        let has_ctl_queue = (feat & VIRTIO_NET_F_CTRL_VQ) != 0;
        if (feat & VIRTIO_NET_F_MQ) != 0 {
            self.hdl.set_pairs(PROPOLIS_MAX_MQ_PAIRS).unwrap();
        }
        // Queue count is a NonZeroU16; hence `get` and -1 will not underflow.
        let io_queues =
            self.virtio_state.queues.count().get() - u16::from(has_ctl_queue);
        let pairs = io_queues / 2;
        if !io_queues.is_multiple_of(2) {
            return Err(MigrateStateError::ImportFailed(format!(
                "source IO queue count was not even: {io_queues}"
            )));
        }
        probes::virtio_viona_mq_set_use_pairs!(|| (
            MqSetPairsCause::Import as u8,
            pairs
        ));
        self.hdl.set_usepairs(pairs).map_err(|e| {
            MigrateStateError::ImportFailed(format!(
                "error while restoring use pairs ({pairs}): {e:?}"
            ))
        })?;

        let input: migrate::VionaStateV1 = match offer.take() {
            Ok(input) => input,
            // A source predating this payload has no guest Rx state to
            // restore.
            Err(MigrateStateError::DataMissing) => {
                return Ok(());
            }
            Err(e) => return Err(e),
        };

        if matches!(input.promisc, migrate::PromiscMode::AllVlan)
            && migrate::PromiscMode::from(self.inner.lock().unwrap().promisc)
                != migrate::PromiscMode::AllVlan
        {
            return Err(MigrateStateError::ImportFailed(
                "source promiscuity was all-VLAN, which this destination \
                does not support"
                    .to_string(),
            ));
        }

        let migrate::VionaStateV1 {
            // Do not restore the source's promisc level. It is derived from
            // guest state and source-host capabilities.
            //
            // The destination must compute its own level.
            promisc: _,
            filter,
            unicast_mac_filters,
            multicast_mac_filters,
            multicast_table_managed,
        } = input;

        let filter = FilterState::from_bits(filter).ok_or_else(|| {
            MigrateStateError::ImportFailed(format!(
                "unrecognised flags in filter state: {:x}",
                filter & !FilterState::all().bits()
            ))
        })?;

        if filter.intersects(FilterState::RX_EXTRA_CMDS) {
            return Err(MigrateStateError::ImportFailed(format!(
                "unsupported RX_EXTRA flags in filter state: {:x}",
                filter.intersection(FilterState::RX_EXTRA_CMDS).bits()
            )));
        }

        let multicast_mac_filters = multicast_mac_filters
            .into_iter()
            .map(|mac| {
                MulticastMacAddr::try_from(mac).map_err(|mac| {
                    MigrateStateError::ImportFailed(format!(
                        "unicast entry {mac:?} in the multicast filter table"
                    ))
                })
            })
            .collect::<Result<Box<[_]>, _>>()?;

        let mut state = self.inner.lock().unwrap();
        state.filter = filter;
        state.unicast_mac_filters = unicast_mac_filters.into();
        state.multicast_mac_filters = multicast_mac_filters;
        state.mac_table_set = multicast_table_managed;

        // Recompute the Rx configuration from the imported guest state.
        self.reconcile_rx_and_release(state, RxReconcileCause::TableReplaced)
            .map_err(|e| {
                MigrateStateError::ImportFailed(format!(
                    "could not apply imported Rx configuration: {e}"
                ))
            })
    }
}

#[derive(Copy, Clone, Eq, PartialEq, Debug)]
enum NetReg {
    Mac,
    Status,
    MaxVqPairs,
    Mtu,
    Speed,
    Duplex,
    RssMaxKeySize,
    RssMaxIndirectionTableLen,
    SupportedHashTypes,
}
lazy_static! {
    static ref NET_DEV_REGS: RegMap<NetReg> = {
        let layout = [
            (NetReg::Mac, 6),
            (NetReg::Status, 2),
            (NetReg::MaxVqPairs, 2),
            (NetReg::Mtu, 2),
            (NetReg::Speed, 4),
            (NetReg::Duplex, 1),
            (NetReg::RssMaxKeySize, 1),
            (NetReg::RssMaxIndirectionTableLen, 2),
            (NetReg::SupportedHashTypes, 4),
        ];
        RegMap::create_packed(VIRTIO_NET_CFG_SIZE, &layout, None)
    };
}

use viona_api::VionaFd;

impl From<&VirtQueue> for viona_api::vioc_ring_init_modern {
    fn from(vq: &VirtQueue) -> viona_api::vioc_ring_init_modern {
        let id = vq.id;
        let size = vq.size();
        let state = vq.get_state();
        let desc_addr = state.mapping.desc_addr;
        let avail_addr = state.mapping.avail_addr;
        let used_addr = state.mapping.used_addr;
        viona_api::vioc_ring_init_modern {
            rim_index: id,
            rim_qsize: size,
            rim_qaddr_desc: desc_addr,
            rim_qaddr_avail: avail_addr,
            rim_qaddr_used: used_addr,
            ..Default::default()
        }
    }
}

impl From<&VirtQueue> for viona_api::vioc_ring_state {
    fn from(vq: &VirtQueue) -> viona_api::vioc_ring_state {
        let id = vq.id;
        let size = vq.size();
        let state = vq.get_state();
        let desc_addr = state.mapping.desc_addr;
        let avail_addr = state.mapping.avail_addr;
        let used_addr = state.mapping.used_addr;
        let avail_idx = state.avail_idx;
        let used_idx = state.used_idx;
        viona_api::vioc_ring_state {
            vrs_index: id,
            vrs_qsize: size,
            vrs_qaddr_desc: desc_addr,
            vrs_qaddr_avail: avail_addr,
            vrs_qaddr_used: used_addr,
            vrs_used_idx: used_idx,
            vrs_avail_idx: avail_idx,
        }
    }
}

struct VionaHdl(VionaFd, #[cfg(test)] std::sync::atomic::AtomicUsize);
impl VionaHdl {
    fn new(link_id: u32, vm_fd: RawFd) -> io::Result<Self> {
        let vfd = VionaFd::new(link_id, vm_fd)?;

        Ok(Self(
            vfd,
            #[cfg(test)]
            Default::default(),
        ))
    }
    fn delete(&self) -> io::Result<()> {
        self.0.ioctl_usize(viona_api::VNA_IOC_DELETE, 0)?;
        Ok(())
    }
    fn get_avail_features(&self) -> io::Result<u64> {
        let mut features = 0;
        unsafe {
            self.0.ioctl(viona_api::VNA_IOC_GET_FEATURES, &mut features)?;
        }
        Ok(features)
    }
    fn set_features(&self, mut features: u64) -> io::Result<()> {
        unsafe {
            self.0.ioctl(viona_api::VNA_IOC_SET_FEATURES, &mut features)?;
        }
        Ok(())
    }
    fn set_pairs(&self, npairs: u16) -> io::Result<()> {
        self.0.ioctl_usize(viona_api::VNA_IOC_SET_PAIRS, npairs as usize)?;
        Ok(())
    }
    fn set_usepairs(&self, npairs: u16) -> io::Result<()> {
        self.0.ioctl_usize(viona_api::VNA_IOC_SET_USEPAIRS, npairs as usize)?;
        Ok(())
    }
    fn ring_init(&self, vq: &VirtQueue) -> io::Result<()> {
        if !vq.is_control() {
            let mut vna_ring_init = viona_api::vioc_ring_init_modern::from(vq);

            // Gross: `VNA_IOC_RING_INIT*` will have viona go and create an LWP
            // in our process for the vring worker. It will inherit the current
            // LWP's processor binding. ring_init is typically called from a
            // vCPU LWP in service of an MMIO to activate the ring. These facts
            // collaborate to get the worker LWP bound to the same host CPU as
            // happens to be running the vCPU that set the NIC running.
            //
            // Because the guest probably goes and enables all the rings on one
            // CPU as part of some driver operation, if we don't intervene here
            // it's likely all the worker threads for the vNIC will be bound to
            // the same core. We don't actually know, from the device side, if
            // the guest will go and set up the rest of the rings right now, so
            // we have to do the unbind/bind dance for each ring.
            //
            // Arguably one might not want to do such operations directly on a
            // vCPU thread. Device setup isn't exactly on anyone's hot path so
            // we'll live.
            pbind::with_unbound_lwp(|| unsafe {
                self.0.ioctl(
                    viona_api::VNA_IOC_RING_INIT_MODERN,
                    &mut vna_ring_init,
                )
            })?;
        }
        Ok(())
    }
    fn ring_reset(&self, vq: &VirtQueue) -> io::Result<()> {
        if !vq.is_control() {
            let idx = vq.id as usize;
            self.0.ioctl_usize(viona_api::VNA_IOC_RING_RESET, idx)?;
        }
        Ok(())
    }
    fn ring_kick(&self, vq: &VirtQueue) -> io::Result<()> {
        if !vq.is_control() {
            let idx = vq.id as usize;
            self.0.ioctl_usize(viona_api::VNA_IOC_RING_KICK, idx)?;
        }
        Ok(())
    }
    fn ring_pause(&self, vq: &VirtQueue) -> io::Result<()> {
        if !vq.is_control() {
            let idx = vq.id as usize;
            self.0.ioctl_usize(viona_api::VNA_IOC_RING_PAUSE, idx)?;
        }
        Ok(())
    }
    fn ring_set_state(&self, vq: &VirtQueue) -> io::Result<()> {
        if !vq.is_control() {
            let mut cfg = viona_api::vioc_ring_state::from(vq);
            unsafe {
                self.0.ioctl(viona_api::VNA_IOC_RING_SET_STATE, &mut cfg)?;
            }
        }
        Ok(())
    }
    fn ring_get_state(&self, vq: &VirtQueue) -> io::Result<queue::Info> {
        let mut cfg = viona_api::vioc_ring_state {
            vrs_index: vq.id,
            ..Default::default()
        };
        if !vq.is_control() {
            unsafe {
                self.0.ioctl(viona_api::VNA_IOC_RING_GET_STATE, &mut cfg)?;
            }
        }
        Ok(queue::Info {
            mapping: queue::MapInfo {
                desc_addr: cfg.vrs_qaddr_desc,
                avail_addr: cfg.vrs_qaddr_avail,
                used_addr: cfg.vrs_qaddr_used,
                valid: true,
            },
            avail_idx: cfg.vrs_avail_idx,
            used_idx: cfg.vrs_used_idx,
        })
    }
    fn ring_cfg_msi(
        &self,
        vq: &VirtQueue,
        cfg: Option<VqIntr>,
    ) -> io::Result<()> {
        if !vq.is_control() {
            let (addr, msg) = match cfg {
                Some(VqIntr::Msi(a, m, masked)) if !masked => (a, m),
                // If MSI is disabled, or the entry is masked (individually,
                // or at the function level), then disable in-kernel
                // acceleration of MSI delivery.
                _ => (0, 0),
            };

            let mut vna_ring_msi = viona_api::vioc_ring_msi {
                rm_index: vq.id,
                _pad: [0; 3],
                rm_addr: addr,
                rm_msg: u64::from(msg),
            };
            unsafe {
                self.0.ioctl(
                    viona_api::VNA_IOC_RING_SET_MSI,
                    &mut vna_ring_msi,
                )?;
            }
        }
        Ok(())
    }
    fn intr_poll(
        &self,
        max_intrs: usize,
        mut f: impl FnMut(u16),
    ) -> io::Result<()> {
        let mut vna_ip = viona_api::vioc_intr_poll_mq::default();
        vna_ip.vipm_nrings = max_intrs as u16;
        let mut nintrs = unsafe {
            self.0.ioctl(viona_api::VNA_IOC_INTR_POLL_MQ, &mut vna_ip)?
        };
        let nrings = vna_ip.vipm_nrings as usize;
        for i in 0..nrings {
            let k = i / 32;
            let b = i % 32;
            if vna_ip.vipm_status[k].get_bit(b) {
                f(i as u16);
                nintrs -= 1;
                if nintrs == 0 {
                    break;
                }
            }
        }
        Ok(())
    }
    fn ring_intr_clear(&self, idx: u16) -> io::Result<()> {
        self.0.ioctl_usize(viona_api::VNA_IOC_RING_INTR_CLR, idx as usize)?;
        Ok(())
    }

    /// Get the minor instance number of the viona device.
    /// This is used for matching kernal statistic entries to the viona device.
    fn instance_id(&self) -> io::Result<u32> {
        self.0.instance_id()
    }

    /// Set MTU for viona device
    fn set_mtu(&self, mtu: u16) -> io::Result<()> {
        self.0.ioctl_usize(viona_api::VNA_IOC_SET_MTU, mtu.into())?;
        Ok(())
    }

    fn api_version(&self) -> io::Result<u32> {
        self.0.api_version()
    }

    /// Sets the address that viona recognizes for virtqueue notifications
    ///
    /// Viona can install a hook in the associated VM at a specified address (in
    /// either the guest port or physical address spaces) to recognize guest
    /// writes that notify in-kernel emulated virtqueues of available buffers.
    ///
    /// With a non-zero argument, viona will attempt to attach such a hook,
    /// replacing any currently in place.  When the argument is None, any
    /// existing hook is torn down.
    fn set_notify_io_port(&self, port: Option<NonZeroU16>) -> io::Result<()> {
        self.0.ioctl_usize(
            viona_api::VNA_IOC_SET_NOTIFY_IOP,
            port.map(|p| p.get()).unwrap_or(0) as usize,
        )?;
        Ok(())
    }
    fn set_notify_mmio_addr(&self, addr: Option<u64>) -> io::Result<()> {
        let mut vim = viona_api::vioc_notify_mmio::default();
        let ptr = addr
            .map(|vim_address| {
                vim.vim_address = vim_address;
                vim.vim_size = super::pci::NOTIFY_REG_SIZE as u32;
                &raw mut vim
            })
            .unwrap_or(std::ptr::null_mut());
        unsafe {
            self.0.ioctl(viona_api::VNA_IOC_SET_NOTIFY_MMIO, ptr)?;
        }
        Ok(())
    }

    /// Set the desired promiscuity level on this interface.
    fn set_promisc(&self, p: PromiscLevel) -> io::Result<()> {
        self.0.ioctl_usize(viona_api::VNA_IOC_SET_PROMISC, usize::from(p))?;
        #[cfg(test)]
        self.1.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Ok(())
    }

    /// Replace the multicast MAC filter table installed on the underlying
    /// MAC client.
    ///
    /// The kernel drops broadcast and duplicate entries, then compacts the
    /// table.
    fn set_mac_filters(
        &self,
        multicast: &[MulticastMacAddr],
    ) -> Result<(), MacFilterError> {
        // A count of zero clears the table without the kernel reading the
        // buffer. The ioctl receives a null pointer in that case.
        let ptr = if multicast.is_empty() {
            std::ptr::null()
        } else {
            multicast.as_bytes().as_ptr()
        };
        let requested = multicast.len() as u32;
        let mut vmf = viona_api::vioc_mac_filters {
            vmf_nmcast: requested,
            vmf_addrs: u64::try_from(ptr.addr())
                .expect("usize fits in u64 on 64-bit targets"),
            ..Default::default()
        };

        let res = unsafe {
            self.0.ioctl(viona_api::VNA_IOC_SET_MAC_FILTERS, &mut vmf)
        };

        Self::check_mac_filters(res, &vmf, requested)
    }

    fn check_mac_filters(
        res: io::Result<i32>,
        vmf: &viona_api::vioc_mac_filters,
        requested: u32,
    ) -> Result<(), MacFilterError> {
        let err = match res {
            Ok(_) => match vmf.vmf_err {
                viona_api::VMF_OK => return Ok(()),
                viona_api::VMF_ERR_COUNT => {
                    MacFilterError::Count { capacity: vmf.vmf_nmcast }
                }
                viona_api::VMF_ERR_NOT_MCAST => {
                    MacFilterError::NotMulticast { addr: vmf.vmf_erraddr }
                }
                viona_api::VMF_ERR_INSTALL => {
                    MacFilterError::Install { addr: vmf.vmf_erraddr }
                }
                viona_api::VMF_ERR_NO_UNICAST => MacFilterError::NoUnicast,
                code => MacFilterError::Unknown(code),
            },
            Err(e) => MacFilterError::Io(e),
        };

        probes::virtio_viona_mac_filters_err!(|| err.probe_args(requested));
        Err(err)
    }

    /// Read back the multicast MAC filter table installed on the underlying
    /// MAC client.
    #[cfg(test)]
    fn get_mac_filters(&self) -> io::Result<Vec<MacAddr>> {
        let mut vmf = viona_api::vioc_mac_filters {
            vmf_nmcast: 0,
            vmf_addrs: 0,
            ..Default::default()
        };

        unsafe {
            self.0.ioctl(viona_api::VNA_IOC_GET_MAC_FILTERS, &mut vmf)?;
        }

        let mut addrs = vec![MacAddr::default(); vmf.vmf_nmcast as usize];
        if addrs.is_empty() {
            return Ok(addrs);
        }

        vmf.vmf_nmcast = addrs.len() as u32;
        vmf.vmf_addrs = u64::try_from(addrs.as_mut_ptr().addr())
            .expect("usize fits in u64 on 64-bit targets");
        unsafe {
            self.0.ioctl(viona_api::VNA_IOC_GET_MAC_FILTERS, &mut vmf)?;
        }

        let installed = vmf.vmf_nmcast as usize;
        if installed > addrs.len() {
            return Err(io::Error::new(
                ErrorKind::InvalidData,
                "kernel installed filter count exceeded the readback buffer",
            ));
        }
        addrs.truncate(installed);
        Ok(addrs)
    }
}

impl AsRawFd for VionaHdl {
    fn as_raw_fd(&self) -> RawFd {
        self.0.as_raw_fd()
    }
}

// This is an ugly hack to work around tokio's inability to poll for event
// readiness on states other than POLLIN/POLLOUT, since viona communicates
// changes to in-kernel ring interrupt state with POLLRDBAND.  In the short
// term, we can translate that to POLLIN using nested epoll.  The viona fd is
// added to an epoll handle, subscribing to EPOLLRDBAND.  When that condition is
// met for the device, epoll will generate an event, making the epoll fd itself
// readable.  We can subscribe to that using the normal tokio event system.
//
// In the long term, viona should probably move to something like eventfd to
// make polling on those ring interrupt events more accessible.
struct Poller {
    epfd: RawFd,
    receiver: watch::Receiver<TargetState>,
    dev: Weak<PciVirtioViona>,
    state: Arc<PollerState>,
}

enum TargetState {
    Pause,
    Run,
    Exit,
}
struct PollerState {
    cv: Condvar,
    running: Mutex<bool>,
}
impl PollerState {
    fn wait_stopped(&self) {
        let guard = self.running.lock().unwrap();
        let _res = self.cv.wait_while(guard, |g| *g).unwrap();
    }
    fn wait_running(&self) {
        let guard = self.running.lock().unwrap();
        let _res = self.cv.wait_while(guard, |g| !*g).unwrap();
    }
    fn set_stopped(&self) {
        let mut guard = self.running.lock().unwrap();
        if *guard {
            *guard = false;
            self.cv.notify_all();
        }
    }
    fn set_running(&self) {
        let mut guard = self.running.lock().unwrap();
        if !*guard {
            *guard = true;
            self.cv.notify_all();
        }
    }
}

struct PollerHdl {
    _join: JoinHandle<()>,
    sender: watch::Sender<TargetState>,
    state: Arc<PollerState>,
}

#[cfg(target_os = "illumos")]
impl Poller {
    fn spawn(
        viona_fd: RawFd,
        dev: Weak<PciVirtioViona>,
    ) -> io::Result<PollerHdl> {
        let epfd = unsafe { libc::epoll_create1(libc::EPOLL_CLOEXEC) } as RawFd;
        if epfd == -1 {
            return Err(Error::last_os_error());
        }
        let mut event =
            libc::epoll_event { events: libc::EPOLLRDBAND as u32, u64: 0 };
        let res = unsafe {
            libc::epoll_ctl(epfd, libc::EPOLL_CTL_ADD, viona_fd, &mut event)
        };
        if res == -1 {
            return Err(Error::last_os_error());
        }

        let state = Arc::new(PollerState {
            cv: Condvar::new(),
            running: Mutex::new(false),
        });
        let (sender, receiver) = watch::channel(TargetState::Pause);
        let mut poller = Poller { epfd, receiver, dev, state: state.clone() };

        let _join = tokio::spawn(async move {
            poller.poll_interrupts().await;
            poller.state.set_stopped();
        });

        Ok(PollerHdl { _join, sender, state })
    }
    fn event_present(&self) -> io::Result<bool> {
        let max_events = 1;
        let mut event = libc::epoll_event { events: 0, u64: 0 };
        let res =
            unsafe { libc::epoll_wait(self.epfd, &mut event, max_events, 0) };
        match res {
            -1 => {
                let err = Error::last_os_error();
                if matches!(err.kind(), ErrorKind::Interrupted) {
                    Ok(false)
                } else {
                    Err(err)
                }
            }
            0 => Ok(false),
            x if x == max_events => Ok(true),
            x => {
                panic!("unexpected {} events", x);
            }
        }
    }
    async fn poll_interrupts(&mut self) {
        let afd =
            AsyncFd::with_interest(self.epfd, Interest::READABLE).unwrap();
        loop {
            loop {
                match *self.receiver.borrow_and_update() {
                    TargetState::Exit => return,
                    TargetState::Run => {
                        self.state.set_running();
                        break;
                    }
                    TargetState::Pause => {
                        self.state.set_stopped();
                        // Fall through to wait for next state change
                    }
                }
                if self.receiver.changed().await.is_err() {
                    return;
                }
            }

            tokio::select! {
                readable = afd.readable() => {
                    if readable.is_err() {
                        return;
                    }
                    let mut readable = readable.unwrap();
                    match self.event_present() {
                        Ok(false) => {
                            readable.clear_ready();
                        }
                        Ok(true) => {
                            if let Some(dev) = Weak::upgrade(&self.dev) {
                                dev.process_interrupts();
                            } else {
                                // Underlying device has been dropped
                                return;
                            }
                        }
                        Err(_) => {
                            return;
                        }
                    };
                }
                _state_change = self.receiver.changed() => {
                    // Fall through to the state management above
                }
            }
        }
    }
}

// macOS doesn't expose the epoll_create1 function as well as some other
// constants used above. Given viona isn't available on non-illumos systems
// anyways, we stub with just enough that it builds and can run unit tests.
#[cfg(not(target_os = "illumos"))]
impl Poller {
    fn spawn(
        _viona_fd: RawFd,
        _dev: Weak<PciVirtioViona>,
    ) -> io::Result<PollerHdl> {
        Err(Error::new(
            ErrorKind::Other,
            "viona not available on non-illumos systems",
        ))
    }
}

impl Drop for Poller {
    fn drop(&mut self) {
        unsafe {
            libc::close(self.epfd);
        }
    }
}

pub(crate) mod bits {
    #![allow(unused)]

    pub const VIRTIO_NET_S_LINK_UP: u16 = 1 << 0;
    pub const VIRTIO_NET_S_ANNOUNCE: u16 = 1 << 1;

    pub const VIRTIO_NET_CFG_SIZE: usize = 6 + 2 + 2 + 2 + 4 + 1 + 1 + 2 + 4;
}
use bits::*;

/// Check that available viona API matches expectations of propolis crate.
pub(crate) fn check_api_version() -> Result<(), crate::api_version::Error> {
    let vers = viona_api::api_version()?;
    let want = viona_api::ApiVersion::V7 as u32;

    if vers < want {
        Err(crate::api_version::Error::TooLow { have: vers, want })
    } else {
        Ok(())
    }
}

/// Test functionality of the virtio-nic device as much as seems reasonable
/// without having a full guest and driver running the device. Unless stated
/// otherwise, the test expectations here are not grounded in any kind of
/// observed behavior, just "the VirtIO spec says ... so ..."
///
/// If guests require changes that cause these tests to fail, please note the
/// cirumstances carefully, and consider if these test expectations were correct
/// in the first place; in some sense these tests function as a bespoke
/// "virtio-nic driver" that lives only in Propolis' tests.
#[cfg(test)]
mod test {
    use crate::common::{GuestAddr, RWOp, ReadOp, WriteOp, MB, PAGE_SIZE};
    use crate::hw::chipset::i440fx::{self, I440FxHostBridge};
    use crate::hw::chipset::Chipset;
    use crate::hw::pci;
    use crate::hw::pci::device::Device;
    use crate::hw::pci::Bdf;
    use crate::hw::virtio::pci::Status;
    use crate::hw::virtio::viona::{
        control, MacAddr, MacFilterError, MulticastMacAddr, PromiscLevel,
        VionaHdl, ETHERADDRL, VIRTIO_NET_F_CTRL_RX, VIRTIO_NET_F_CTRL_VQ,
        VIRTIO_NET_F_MAC, VIRTIO_NET_F_MQ, VIRTIO_NET_F_STATUS,
    };
    use crate::hw::virtio::{PciVirtioViona, VirtioDevice};
    use crate::lifecycle::Lifecycle;
    use crate::migrate::{
        MigrateCtx, MigrateMulti, MigrateStateError, PayloadOffer,
        PayloadOffers, PayloadOutputs, Schema,
    };
    use crate::Machine;
    use std::collections::BTreeMap;
    use std::env::VarError;
    use std::process::Command;
    use std::sync::Arc;

    struct TestCtx {
        test_name: &'static str,
        underlying_nic: String,
        vnic_name: String,
        machine: Machine,
        dev: Arc<PciVirtioViona>,
    }

    impl Drop for TestCtx {
        fn drop(&mut self) {
            Lifecycle::pause(self.dev.as_ref());
            Lifecycle::halt(self.dev.as_ref());
        }
    }

    impl TestCtx {
        fn create_driver(&self) -> VirtioNetDriver<'_, '_> {
            VirtioNetDriver::for_hardware(&self.machine, &self.dev)
        }

        fn migrate(self) -> TestCtx {
            let payloads = export_payloads(&self);
            let new_ctx = recreate_ctx(self);
            import_payloads(&new_ctx, &payloads)
                .expect("can import PciVirtioViona");
            Lifecycle::start(new_ctx.dev.as_ref())
                .expect("can start viona device");
            new_ctx
        }
    }

    fn export_payloads(ctx: &TestCtx) -> Vec<(String, u32, String)> {
        let mut dev_payloads = PayloadOutputs::new();
        let acc_mem = ctx.machine.acc_mem.access().expect("machine has memory");
        let mctx = MigrateCtx { mem: &acc_mem };

        <PciVirtioViona>::export(&ctx.dev, &mut dev_payloads, &mctx)
            .expect("can export PciVirtioViona");
        dev_payloads
            .into_iter()
            .map(|output| {
                let bytes = serde_json::to_string(&output.payload)
                    .expect("serializing payload output");
                (output.kind.to_string(), output.version, bytes)
            })
            .collect()
    }

    fn import_payloads(
        ctx: &TestCtx,
        payloads: &[(String, u32, String)],
    ) -> Result<(), MigrateStateError> {
        let mut desers: Vec<_> = payloads
            .iter()
            .map(|(_, _, bytes)| serde_json::Deserializer::from_str(bytes))
            .collect();
        let mut offers =
            PayloadOffers::new(payloads.iter().zip(desers.iter_mut()).map(
                |((kind, version, _bytes), deser)| PayloadOffer {
                    kind: kind.as_str(),
                    version: *version,
                    payload: Box::new(<dyn erased_serde::Deserializer>::erase(
                        deser,
                    )),
                },
            ));
        let acc_mem = ctx.machine.acc_mem.access().expect("machine has memory");
        let mctx = MigrateCtx { mem: &acc_mem };
        <PciVirtioViona>::import(&ctx.dev, &mut offers, &mctx)
    }

    fn recreate_ctx(test_ctx: TestCtx) -> TestCtx {
        let vnic_name = test_ctx.vnic_name.clone();
        let underlying_nic = test_ctx.underlying_nic.clone();
        let test_name = test_ctx.test_name;
        drop(test_ctx);
        delete_vnic(&vnic_name);
        create_vnic(&underlying_nic, &vnic_name);
        create_test_ctx(test_name, &underlying_nic, &vnic_name)
    }

    fn create_test_ctx(
        test_name: &'static str,
        underlying_nic: &str,
        vnic_name: &str,
    ) -> TestCtx {
        // Create the VM with `force: true`: if we're running tests concurrently
        // this will trample an existing test (which should then fail!). We do
        // this so that if a test misconfiguration left a stray old VM hanging
        // around we'll get it out of the way for this test re-run.
        //
        // No reservoir because the test VM is tiny and we don't want to require
        // even more specific host configuration for tests. There's no reason
        // the reservoir should be affecting virtio-nic-related tests anyway.
        let vm_opts = crate::vmm::CreateOpts {
            force: true,
            use_reservoir: false,
            track_dirty: false,
        };
        let vm_name = format!("virtio-viona-test-{}", test_name);
        let machine = crate::vmm::Builder::new(&vm_name, vm_opts)
            .expect("can set up vmm builder")
            .add_mem_region(0, 64 * MB, "test mem")
            .expect("can add dummy mem region")
            .max_cpus(1)
            .expect("can add cpus")
            .finalize()
            .expect("can create test VMM");
        let pci_topology = pci::topology::Builder::new()
            .finish(&machine)
            .expect("can build empty topology")
            .topology;
        let chipset_hb = I440FxHostBridge::create(
            pci_topology,
            i440fx::Opts {
                power_pin: None,
                reset_pin: None,
                enable_pcie: false,
            },
        );
        let viona_dev = PciVirtioViona::new(vnic_name, &machine.hdl, None)
            .expect("can create test vnic");

        chipset_hb.pci_attach(i440fx::DEFAULT_HB_BDF, chipset_hb.clone(), None);
        chipset_hb.attach(&machine);
        chipset_hb.pci_attach(
            Bdf::new_unchecked(0, 8, 0),
            viona_dev.clone(),
            None,
        );

        TestCtx {
            machine,
            dev: viona_dev,
            test_name,
            underlying_nic: underlying_nic.to_owned(),
            vnic_name: vnic_name.to_owned(),
        }
    }

    /// Glue for a nicer test interface to read/write a specific structure in a
    /// PCI BAR.
    struct BarAccessor<'dev> {
        dev: &'dev PciVirtioViona,
        bar: pci::BarN,
        offset: usize,
    }

    impl<'dev> BarAccessor<'dev> {
        fn at(
            dev: &'dev PciVirtioViona,
            bar: pci::BarN,
            offset: usize,
        ) -> Self {
            Self { dev, bar, offset }
        }

        fn read(&self, addr: usize, buf: &mut [u8]) {
            let mut op = ReadOp::from_buf(self.offset + addr, buf);
            self.dev.bar_rw(self.bar, RWOp::Read(&mut op));
        }

        fn write(&self, addr: usize, buf: &[u8]) {
            let mut op = WriteOp::from_buf(self.offset + addr, buf);
            self.dev.bar_rw(self.bar, RWOp::Write(&mut op));
        }

        fn read_u8(&self, addr: usize) -> u8 {
            let mut b = [0];
            self.read(addr, &mut b);
            b[0]
        }

        fn read_le16(&self, addr: usize) -> u16 {
            let mut b = [0, 0];
            self.read(addr, &mut b);
            u16::from_le_bytes(b)
        }

        fn read_le32(&self, addr: usize) -> u32 {
            let mut b = [0, 0, 0, 0];
            self.read(addr, &mut b);
            u32::from_le_bytes(b)
        }

        fn write_u8(&self, addr: usize, v: u8) {
            self.write(addr, &[v]);
        }

        fn write_le16(&self, addr: usize, v: u16) {
            self.write(addr, &v.to_le_bytes());
        }

        fn write_le32(&self, addr: usize, v: u32) {
            self.write(addr, &v.to_le_bytes());
        }

        fn write_le64(&self, addr: usize, v: u64) {
            self.write(addr, &v.to_le_bytes());
        }
    }

    /// `COMMON_REGS` describes the common configuration structure for VirtIO
    /// devices, but that machinery is oriented around translating access
    /// offsets into a structured enum variant. In these tests though, we'll go
    /// from desired field access to offsets in an RWOp.
    ///
    /// This namespace gives names for various field offsets, matching
    /// `COMMON_REGS` and its source, `struct virtio_pci_common_cfg` from the
    /// VirtIO spec.
    // Items here are named to match struct fields from the VirtIO spec.
    #[allow(non_upper_case_globals, dead_code)]
    mod common_cfg {
        // > /* About the whole device. */
        pub const device_feature_select: usize = 0;
        pub const device_feature: usize = 4;
        pub const driver_feature_select: usize = 8;
        pub const driver_feature: usize = 12;
        pub const config_msix_vector: usize = 16;
        pub const num_queues: usize = 18;
        pub const device_status: usize = 20;
        pub const config_generation: usize = 21;

        // > /* About a specific virtqueue. */
        pub const queue_select: usize = 22;
        pub const queue_size: usize = 24;
        pub const queue_msix_vector: usize = 26;
        pub const queue_enable: usize = 28;
        pub const queue_notify_off: usize = 30;
        pub const queue_desc: usize = 32;
        pub const queue_driver: usize = 40;
        pub const queue_device: usize = 48;
        pub const queue_notify_data: usize = 56;
        pub const queue_reset: usize = 58;
    }

    #[allow(non_upper_case_globals, dead_code)]
    mod net_config {
        pub const mac: usize = 0;
        pub const status: usize = 6;
        // This field is only valid if VIRTIO_NET_F_MQ is negotiated.
        pub const max_virtqueue_pairs: usize = 8;
        // This field is only valid if VIRTIO_NET_F_MTU is negotiated.
        pub const mtu: usize = 10;
        // This and `duplex` are only valid if VIRTIO_NET_F_SPEED_DUPLEX is
        // negotiated.
        pub const speed: usize = 12;
        pub const duplex: usize = 16;
        // These fields all depend on VIRTIO_NET_F_RSS or related features,
        // which we won't set for these tests..
        pub const rss_max_key_size: usize = 17;
        pub const rss_max_indirection_table_length: usize = 18;
        pub const supported_hash_types: usize = 20;
    }

    #[test]
    fn test_common_cfg_size_is_right() {
        // TODO: in a more recent rust this could be a `const { assert_eq!() }`
        assert_eq!(
            common_cfg::queue_reset + 2,
            crate::hw::virtio::pci::COMMON_REG_SIZE_TEST
        )
    }

    #[test]
    fn test_mac_addr_carries_no_padding() {
        assert_eq!(std::mem::size_of::<MacAddr>(), ETHERADDRL);
        assert_eq!(std::mem::size_of::<MulticastMacAddr>(), ETHERADDRL);
    }

    #[test]
    fn test_multicast_mac_addr_admits_group_bit_only() {
        let unicast = MacAddr::from([0x02, 0x08, 0x20, 0xac, 0x70, 0x99]);
        assert_eq!(MulticastMacAddr::try_from(unicast), Err(unicast));

        let broadcast = MacAddr::from([0xff; ETHERADDRL]);
        assert!(MulticastMacAddr::try_from(broadcast).is_ok());
    }

    #[test]
    fn test_mcast_table_actions() {
        use super::McastTableAction::{self, Clear, Install, Keep};
        use super::RxReconcileCause::{
            FilterChange, Reinitialize, TableReplaced,
        };

        for (case, (wanted, installed, cause, expected)) in [
            (false, false, FilterChange, Keep),
            (false, true, FilterChange, Clear),
            (true, false, FilterChange, Install),
            (true, true, FilterChange, Keep),
            (false, false, TableReplaced, Keep),
            (false, true, TableReplaced, Clear),
            (true, false, TableReplaced, Install),
            (true, true, TableReplaced, Install),
            (false, false, Reinitialize, Clear),
            (false, true, Reinitialize, Clear),
            (true, false, Reinitialize, Clear),
            (true, true, Reinitialize, Clear),
        ]
        .into_iter()
        .enumerate()
        {
            assert_eq!(
                McastTableAction::derive(wanted, installed, &cause),
                expected,
                "case {case}: wanted={wanted}, installed={installed}",
            );
        }
    }

    #[test]
    fn test_mac_filters_semantic_results() {
        let requested = 65;
        let addr = [0x02, 0x08, 0x20, 0xac, 0x70, 0x99];
        for code in [
            viona_api::VMF_OK,
            viona_api::VMF_ERR_COUNT,
            viona_api::VMF_ERR_NOT_MCAST,
            viona_api::VMF_ERR_INSTALL,
            viona_api::VMF_ERR_NO_UNICAST,
            5,
            u32::MAX,
        ] {
            let vmf = viona_api::vioc_mac_filters {
                vmf_nmcast: 17,
                vmf_err: code,
                vmf_erraddr: addr,
                ..Default::default()
            };
            let outcome = VionaHdl::check_mac_filters(Ok(0), &vmf, requested);
            match (code, &outcome) {
                (viona_api::VMF_OK, Ok(())) => {}
                (
                    viona_api::VMF_ERR_COUNT,
                    Err(MacFilterError::Count { capacity }),
                ) => assert_eq!(*capacity, 17),
                (
                    viona_api::VMF_ERR_NOT_MCAST,
                    Err(MacFilterError::NotMulticast { addr: actual }),
                )
                | (
                    viona_api::VMF_ERR_INSTALL,
                    Err(MacFilterError::Install { addr: actual }),
                ) => assert_eq!(*actual, addr),
                (
                    viona_api::VMF_ERR_NO_UNICAST,
                    Err(MacFilterError::NoUnicast),
                ) => {}
                (5 | u32::MAX, Err(MacFilterError::Unknown(actual))) => {
                    assert_eq!(*actual, code)
                }
                _ => {
                    panic!("unexpected result for vmf_err={code}: {outcome:?}")
                }
            }
            if let Err(error) = &outcome {
                let (addr, count) = match code {
                    viona_api::VMF_ERR_COUNT => (0, 17),
                    viona_api::VMF_ERR_NOT_MCAST
                    | viona_api::VMF_ERR_INSTALL => {
                        (0x0000_0208_20ac_7099, requested)
                    }
                    _ => (0, requested),
                };
                assert_eq!(error.probe_args(requested), (code, addr, count, 0));
            }
        }
    }

    #[test]
    fn test_mac_filters_ioctl_error_overrides_semantic_result() {
        let requested = 65;
        let vmf = viona_api::vioc_mac_filters {
            vmf_nmcast: 17,
            vmf_err: viona_api::VMF_ERR_INSTALL,
            vmf_erraddr: [0xff; ETHERADDRL],
            ..Default::default()
        };
        for error in [
            std::io::Error::from_raw_os_error(libc::EFAULT),
            std::io::Error::other("test ioctl failure"),
        ] {
            let expected_errno = error.raw_os_error();
            let expected_kind = error.kind();
            let expected_message = error.to_string();
            let error =
                VionaHdl::check_mac_filters(Err(error), &vmf, requested)
                    .unwrap_err();
            assert_eq!(
                error.probe_args(requested),
                (viona_api::VMF_OK, 0, requested, expected_errno.unwrap_or(0)),
            );
            let MacFilterError::Io(error) = error else {
                panic!("expected ioctl failure: {error:?}");
            };
            assert_eq!(error.raw_os_error(), expected_errno);
            assert_eq!(error.kind(), expected_kind);
            assert_eq!(error.to_string(), expected_message);
        }
    }

    #[test]
    fn test_viona_state_v1_multicast_table_managed_default() {
        let prev_payload = r#"{
            "promisc": "AllMulti",
            "filter": 0,
            "unicast_mac_filters": [],
            "multicast_mac_filters": [[1, 0, 94, 0, 0, 1]]
        }"#;

        let state: super::migrate::VionaStateV1 =
            serde_json::from_str(prev_payload)
                .expect("payload without multicast_table_managed deserializes");
        assert!(!state.multicast_table_managed);
        assert_eq!(state.promisc, super::migrate::PromiscMode::AllMulti);
        assert_eq!(state.multicast_mac_filters.len(), 1);
    }

    #[test]
    fn test_viona_state_v1_export_raises_promisc_for_pre_filter_targets() {
        use super::migrate::VionaStateV1;

        let all_hosts = MacAddr::from([0x01, 0x00, 0x5e, 0x00, 0x00, 0x01]);
        let mut state = super::Inner::new(0, PromiscLevel::None);
        state.mac_table_set = true;
        state.multicast_mac_filters =
            Box::new([MulticastMacAddr::try_from(all_hosts).unwrap()]);
        state.mac_filters_installed = true;

        let exported = VionaStateV1::from(&state);
        let serialized = serde_json::to_value(&exported).unwrap();
        assert_eq!(serialized["promisc"], "AllMulti");
        assert_eq!(exported.multicast_mac_filters, vec![all_hosts]);
        assert!(exported.multicast_table_managed);
        assert_eq!(state.promisc, PromiscLevel::None);

        state.multicast_mac_filters = Box::new([]);
        state.mac_filters_installed = false;

        let exported = VionaStateV1::from(&state);
        let serialized = serde_json::to_value(&exported).unwrap();
        assert_eq!(serialized["promisc"], "None");
        assert!(exported.multicast_mac_filters.is_empty());
        assert!(exported.multicast_table_managed);
    }

    /// A very simple "driver" to drive test operations on a VirtIO device based
    /// on our understanding of the VirtIO spec.
    ///
    /// This serves as a stand-in for some kind of guest software initializing
    /// (and potentially one day?) operating a virtio-nic device. Tests using
    /// this "driver" will often instantiate it multiple times as an
    /// approximation of various guest operating systems initializing their
    /// distinct drivers.
    struct VirtioNetDriver<'mach, 'nic> {
        machine: &'mach Machine,
        dev: &'nic PciVirtioViona,
        common_config: BarAccessor<'nic>,
        device_config: BarAccessor<'nic>,
        state: DriverState,
    }

    /// The "volatile" part of `VirtioNetDriver`: whatever "guest-side" part
    /// should remain constant when tests migrate the corresponding
    /// `PciVirtioViona`.
    //
    // Theoretically this state could live in the test VM, but that would
    // require "migrating" guest memory, which is more work than is strictly
    // necessary to test the de vice. On top of that it's a bit annoying to
    // fulfill "driver memory" as reads/writes into the test VM, so we don't.
    struct DriverState {
        max_pairs: Option<u16>,
        next_queue_gpa: u64,
        queue_locs: BTreeMap<u16, QueueLoc>,
    }

    /// Guest-physical layout of a virtqueue as programmed by `init_queue`.
    #[derive(Copy, Clone)]
    struct QueueLoc {
        desc_gpa: u64,
        avail_gpa: u64,
        used_gpa: u64,
        size: u16,
        /// The next available-ring index the driver will publish.
        avail_idx: u16,
    }

    fn multicast_table(count: usize) -> Vec<MulticastMacAddr> {
        assert!(count <= u16::MAX as usize);
        (0..count)
            .map(|i| {
                MacAddr::from([0x01, 0x00, 0x5e, 0x00, (i >> 8) as u8, i as u8])
                    .try_into()
                    .expect("generated entry is multicast")
            })
            .collect()
    }

    fn discover_multicast_capacity(hdl: &VionaHdl) -> usize {
        assert!(
            hdl.get_mac_filters()
                .expect("can read filters before capacity probe")
                .is_empty(),
            "capacity probe requires an empty kernel filter table",
        );
        let mut count = viona_api::VIONA_MAX_MCAST_FILTERS;
        loop {
            match hdl.set_mac_filters(&multicast_table(count)) {
                Ok(()) => {
                    hdl.set_mac_filters(&[])
                        .expect("can clear capacity probe table");
                    count = count
                        .checked_mul(2)
                        .expect("multicast capacity probe overflow");
                }
                Err(MacFilterError::Count { capacity }) => {
                    return capacity as usize
                }
                Err(e) => {
                    panic!("multicast capacity probe failed: {e:?}")
                }
            }
        }
    }

    impl DriverState {
        fn new() -> Self {
            Self {
                max_pairs: None,
                // Start virtio-nic queues somewhere other than address 0.
                next_queue_gpa: 2 * MB as u64,
                queue_locs: BTreeMap::new(),
            }
        }
    }

    impl<'mach, 'nic> VirtioNetDriver<'mach, 'nic> {
        fn for_hardware(
            machine: &'mach Machine,
            dev: &'nic PciVirtioViona,
        ) -> Self {
            Self::import(machine, dev, DriverState::new())
        }

        fn set_max_pairs(&mut self, pairs: Option<u16>) {
            self.state.max_pairs = pairs;
        }

        fn import(
            machine: &'mach Machine,
            dev: &'nic PciVirtioViona,
            state: DriverState,
        ) -> Self {
            // We place virtio_pci_common_cfg at BAR 2, offset 0, so hardcode this
            // in the test for now.
            //
            // TODO: it would be more appropriate to walk through the device's PCI
            // capabilities until we find VIRTIO_PCI_CAP_COMMON_CFG but that's a
            // little annoying..
            let common_config = BarAccessor::at(dev, pci::BarN::BAR2, 0);

            // Device-specific configuration offsets above are declared on their
            // own, so even though this is in the same BAR we'll set the base offset
            // to match.
            let device_config =
                BarAccessor::at(dev, pci::BarN::BAR2, PAGE_SIZE);

            Self { machine, dev, common_config, device_config, state }
        }

        fn export(self) -> DriverState {
            self.state
        }

        fn read_status(&self) -> Status {
            Status::from_bits(
                self.common_config.read_u8(common_cfg::device_status),
            )
            .unwrap()
        }

        fn write_status(&self, bits: Status) {
            self.common_config.write_u8(common_cfg::device_status, bits.bits());
        }

        fn set_status_bits(&self, bits: Status) {
            self.write_status(self.read_status() | bits);
        }

        fn status_ok(&self) -> bool {
            !self.read_status().intersects(Status::NEEDS_RESET | Status::FAILED)
        }

        fn ctl_qidx(&self) -> Option<u16> {
            let ctl_queues: Vec<_> = self
                .dev
                .virtio_state
                .queues
                .iter()
                .filter_map(|v| v.is_control().then_some(v.id))
                .collect();
            assert!(
                ctl_queues.len() <= 1,
                "virtio NICs have at most one control queue"
            );
            ctl_queues.get(0).copied()
        }

        // Modern and legacy queue layout requirements differ a bit, but this
        // sets up queues in the legacy format to be usable in both contexts.
        //
        // Descriptor tables begin uninitialized. The MAC-filter
        // control-queue tests later write real descriptor chains into this
        // layout (see `send_ctrl_cmd`). The data queues are never used
        // beyond their layout.
        fn init_queue(&mut self, queue: u16) {
            // Linux's setup_vq checks that the queue index is valid compared to
            // the advertised `num_queues`, regardless of whether or not the
            // device will handle it.
            assert!(
                queue < self.common_config.read_le16(common_cfg::num_queues)
            );

            self.common_config.write_le16(common_cfg::queue_select, queue);

            // We don't strictly *need* to check if the queue was already
            // active, but Linux does (setup_vq()->vp_modern_get_queue_enable())
            // and it is true that we should not be initializing already-enabled
            // queues. So we check here too.
            let already_enabled =
                self.common_config.read_le16(common_cfg::queue_enable) == 1;
            assert!(!already_enabled);

            let queue_size =
                self.common_config.read_le16(common_cfg::queue_size);
            assert_ne!(queue_size, 0);
            // In "2.7 Split Virtqueues",
            //
            // > The maximum Queue Size value is 32768.
            assert!(queue_size <= 32 * 1024);

            let page_u16: u16 = PAGE_SIZE.try_into().unwrap();
            let page_u64: u64 = PAGE_SIZE.try_into().unwrap();

            // For simplicity, shrink `queue_size` small enough that it fits in
            // one page. There are a few additional items for the various parts
            // of virtquues in addition to just an array of 16-byte elements, so
            // we use the next smaller power of two so we round up to one page
            // in the end.
            //
            // TODO: with support for VIRTIO_F_RING_PACKED we will be freed from
            // having to write power of 2 sizes
            let chosen_size = (page_u16 / 16) >> 1;
            if chosen_size < queue_size {
                self.common_config
                    .write_le16(common_cfg::queue_size, chosen_size);
            }

            let acc_mem =
                self.machine.acc_mem.access().expect("can access memory");

            let size = queue_size.min(chosen_size);

            let descriptor_table_gpa = self.state.next_queue_gpa;
            self.common_config
                .write_le64(common_cfg::queue_desc, descriptor_table_gpa);

            // The descriptor "area" ends after 16 bytes per descriptor.
            let desc_len = u64::from(size) * 16;

            let avail_gpa =
                (descriptor_table_gpa + desc_len).next_multiple_of(page_u64);
            // First, flags.
            // > If the VIRTIO_F_EVENT_IDX feature bit is not negotiated:
            // > * The driver MUST set flags to 0 or 1.
            // > * The driver MAY set flags to 1 to advise the device that
            //     notifications are not needed.
            acc_mem.write::<u16>(GuestAddr(avail_gpa), &0);
            // Index. "This starts at 0, and increases."
            acc_mem.write::<u16>(GuestAddr(avail_gpa + 2), &0);
            // Leave all the `ring` entries uninitialized, and we've not
            // negotiated VIRTIO_F_EVENT_IDX so no `used_event` for now.
            self.common_config.write_le64(common_cfg::queue_driver, avail_gpa);

            // The available ring has a 4-byte header and one 2-byte entry per
            // descriptor.
            let avail_len = 4 + 2 * u64::from(size);
            // Place the used ring after the complete available ring.
            let used_gpa = (avail_gpa + avail_len).next_multiple_of(page_u64);

            // The used ring belongs to the device.
            //
            // We zero the memory here rather than rely on fresh guest memory.
            // The completion check in `send_ctrl_cmd` depends on the index
            // starting at 0.
            acc_mem.write::<u16>(GuestAddr(used_gpa), &0);
            acc_mem.write::<u16>(GuestAddr(used_gpa + 2), &0);

            self.common_config.write_le64(common_cfg::queue_device, used_gpa);
            self.common_config.write_le16(common_cfg::queue_enable, 1);

            // The used ring has a 4-byte header and one 8-byte element per
            // descriptor.
            let used_len = 4 + 8 * u64::from(size);

            // Advance beyond the entire used ring before allocating the new
            // queue in the chain.
            self.state.next_queue_gpa =
                (used_gpa + used_len).next_multiple_of(page_u64);

            self.state.queue_locs.insert(
                queue,
                QueueLoc {
                    desc_gpa: descriptor_table_gpa,
                    avail_gpa,
                    used_gpa,
                    size,
                    avail_idx: 0,
                },
            );

            let msi_vector = 0x100 + queue;
            self.common_config
                .write_le16(common_cfg::queue_msix_vector, msi_vector);
            let configured_vector =
                self.common_config.read_le16(common_cfg::queue_msix_vector);
            assert_eq!(configured_vector, msi_vector);
        }

        /// Initialize a VirtIO device according to "Driver Requirements: Device
        /// Initialization". This includes the initial RESET.
        ///
        /// This will panic if device initialization concludes with the device
        /// in NEEDS_RESET.
        fn modern_device_init(&mut self, features: u64) {
            // > The driver MUST follow this sequence to initialize a device:
            // > 1. Reset the device.
            self.write_status(Status::RESET);

            // > 2. Set the ACKNOWLEDGE status bit: the guest OS has noticed the
            // > device.
            self.set_status_bits(Status::ACK);

            // > 3. Set the DRIVER status bit: the guest OS knows how to drive the
            // > device.
            self.set_status_bits(Status::DRIVER);

            // > 4. Read device feature bits, and write the subset of feature bits
            // > understood by the OS and driver to the device. During this step the
            // > driver MAY read (but MUST NOT write) the device-specific
            // > configuration fields to check that it can support the device before
            // > accepting it.
            let device_feats =
                self.common_config.read_le32(common_cfg::device_feature);
            let num_queues =
                self.common_config.read_le16(common_cfg::num_queues);

            // VirtIO defines features as up to 64 bits, but the register is an le32
            // with a separate register to select which part of feature space is to
            // be written. Ignore all this given that no features are defined in the
            // upper space yet (and if they were, we're not using them .. yet..?)
            let features_u32: u32 = features
                .try_into()
                .expect("we don't (yet?) care about features above u32");

            let unsupported = features_u32 & !device_feats;
            if unsupported != 0 {
                panic!(
                    "Test wants more features than the device offers? \n\
                    Device offers: {:#08x}\n\
                    Test wants:    {:#08x}\n\
                    Device lacks:  {:#08x}\n",
                    device_feats, features_u32, unsupported
                );
            }

            // We know that `features` is a subset of `device_feats` by
            // `unsupported` being zero, above.
            eprintln!("writing features: {:#08x}", features_u32);
            self.common_config
                .write_le32(common_cfg::driver_feature, features_u32);

            // > 5. Set the FEATURES_OK status bit. The driver MUST NOT accept new
            // > feature bits after this step.
            self.set_status_bits(Status::FEATURES_OK);

            // > 6. Re-read device status to ensure the FEATURES_OK bit is still
            // > set: otherwise, the device does not support our subset of features
            // > and the device is unusable.
            let device_status = self.read_status();
            if !device_status.contains(Status::FEATURES_OK) {
                // Now, this *really* shouldn't happen, because we've checked that
                // the device just offered up all the features we've requested. But
                // it's possible that some features are mutually-exclusive and we've
                // made a bad choice, in theory..
                panic!(
                    "Device does not support requested features: {:#08x}",
                    features
                );
            }

            // Extra pedantically, the device should not be NEEDS_RESET or FAILED.
            assert!(self.status_ok());

            // > 7. Perform device-specific setup, including discovery of virtqueues
            // > for the device, optional per-bus setup, reading and possibly
            // > writing the device's virtio configuration space, and population of
            // > virtqueues.
            let is_mq = features & VIRTIO_NET_F_MQ != 0;
            let has_control = features & VIRTIO_NET_F_CTRL_VQ != 0;
            assert!(
                !is_mq || has_control,
                "multiqueue requires the control queue feature"
            );

            // We'll configure all of the device's queues here. This is what
            // we've seen both Linux and Windows do with virtio devices (in
            // Linux, virtnet_probe()->init_vqs()). The number of
            // actually-used queues is only configured later.
            let n_qpairs = if is_mq {
                self.device_config.read_le16(net_config::max_virtqueue_pairs)
            } else {
                1
            };
            eprintln!("n_qpairs: {}", n_qpairs);

            // Initialising each required queue will check that its qidx is
            // less than the device's advertised num_queues.
            //
            // Linux enforces this, whereas illumos will overlook this for the
            // control queue iff. we handle the queue operations successfully.
            let n_queues = n_qpairs * 2 + u16::from(has_control);
            for queue in 0..n_queues {
                eprintln!("initializing queue {}", queue);
                self.init_queue(queue);
                assert!(self.status_ok());
            }

            // The last queue we initialise is the control queue. Ensure that
            // the device agrees with us.
            let ctl_qidx = has_control.then_some(n_queues - 1);
            assert_eq!(ctl_qidx, self.ctl_qidx());

            if n_qpairs > 1 {
                self.dev
                    .set_use_pairs(n_qpairs)
                    .expect("can set_use_pairs(max_pairs)");

                // Again following in the footsteps of observed Windows/Linux
                // virtio drivers: now that queues are all initialized, set the
                // number of queue pairs we'll actually use. The test (playing
                // the role of the guest OS) may have selected less than the
                // maximum queue pairs.
                if let Some(wanted_pairs) = self.state.max_pairs {
                    assert!(wanted_pairs <= n_qpairs);
                    self.dev
                        .set_use_pairs(wanted_pairs)
                        .expect("can set_use_pairs(wanted_pairs)");
                }
            }

            // The config-space value num_queues represents the *maximum* number
            // of queues supported by the device, and should not change in
            // response to use_pairs. The control queue should also stay the same,
            // as it depends on max_virtqueue_pairs.
            assert_eq!(
                self.common_config.read_le16(common_cfg::num_queues),
                num_queues
            );
            assert_eq!(ctl_qidx, self.ctl_qidx());

            // From 5.1.4.2 "Driver Requirements: Device configuration layout",
            // > If the driver negotiates VIRTIO_NET_F_MTU, it MUST supply enough
            // > receive buffers to receive at least one receive packet of size mtu
            // > (plus low level ethernet ehader length) with gso_type NONE or ECN.
            //
            // TODO: Does this mean that if we do not provide buffers, but set
            // DRIVER_OK, that the device should fail initialization? huh!

            // > 8. Set the DRIVER_OK status bit. At this point the device is
            // > "live".
            //
            // Is the implication (given 7.) that at this point the the driver
            // should not? must not? write to the device's virtio configuration
            // space?
            self.set_status_bits(Status::DRIVER_OK);

            // Now that the device is initialized we can check once again that it
            // thinks everything is OK...
            assert!(self.status_ok());
        }

        /// Submit a split control-queue command and return its ack.
        ///
        /// The header and payload use readable descriptors.
        ///
        /// The final descriptor contains the device-written ack.
        fn send_ctrl_cmd(
            &mut self,
            class: u8,
            command: u8,
            payload: &[&[u8]],
        ) -> u8 {
            const VIRTQ_DESC_F_NEXT: u16 = 1;
            const VIRTQ_DESC_F_WRITE: u16 = 2;

            let qidx = self.ctl_qidx().expect("device has a control queue");
            let loc = *self
                .state
                .queue_locs
                .get(&qidx)
                .expect("control queue was initialized");

            let header = [class, command];
            let total =
                header.len() + payload.iter().map(|p| p.len()).sum::<usize>();

            let buf_gpa = self.state.next_queue_gpa;
            self.state.next_queue_gpa +=
                (total + 1).next_multiple_of(PAGE_SIZE) as u64;

            let acc_mem =
                self.machine.acc_mem.access().expect("can access memory");

            let mut regions: Vec<(u64, u32, u16)> = Vec::new();
            let mut cursor = buf_gpa;
            for data in
                std::iter::once(&header[..]).chain(payload.iter().copied())
            {
                assert_eq!(
                    acc_mem.write_from(GuestAddr(cursor), data, data.len()),
                    Some(data.len())
                );
                regions.push((cursor, data.len() as u32, 0));
                cursor += data.len() as u64;
            }

            let ack_gpa = cursor;
            // Seed the ack with a value the device will never write.
            assert!(acc_mem.write::<u8>(GuestAddr(ack_gpa), &0xaa));
            regions.push((ack_gpa, 1, VIRTQ_DESC_F_WRITE));

            assert!(regions.len() <= usize::from(loc.size));
            for (i, (gpa, len, flags)) in regions.iter().enumerate() {
                let d = loc.desc_gpa + 16 * i as u64;
                let last = i == regions.len() - 1;
                let flags = flags | if last { 0 } else { VIRTQ_DESC_F_NEXT };
                let next = if last { 0 } else { i as u16 + 1 };
                assert!(acc_mem.write::<u64>(GuestAddr(d), gpa));
                assert!(acc_mem.write::<u32>(GuestAddr(d + 8), len));
                assert!(acc_mem.write::<u16>(GuestAddr(d + 12), &flags));
                assert!(acc_mem.write::<u16>(GuestAddr(d + 14), &next));
            }

            let slot = u64::from(loc.avail_idx % loc.size);
            assert!(acc_mem
                .write::<u16>(GuestAddr(loc.avail_gpa + 4 + 2 * slot), &0u16));
            let new_idx = loc.avail_idx.wrapping_add(1);
            assert!(
                acc_mem.write::<u16>(GuestAddr(loc.avail_gpa + 2), &new_idx)
            );
            self.state.queue_locs.get_mut(&qidx).unwrap().avail_idx = new_idx;

            let vq = self
                .dev
                .virtio_state
                .queues
                .get(qidx)
                .expect("control queue exists")
                .clone();
            self.dev.queue_notify(&vq);

            // Completion happens before reading the ack byte.
            let used_idx = *acc_mem
                .read::<u16>(GuestAddr(loc.used_gpa + 2))
                .expect("can read used index");
            assert_eq!(used_idx, new_idx, "control command was not consumed");

            let used_id = *acc_mem
                .read::<u32>(GuestAddr(loc.used_gpa + 4 + 8 * slot))
                .expect("can read used element id");
            assert_eq!(used_id, 0, "used element reports the chain head");

            let used_len = *acc_mem
                .read::<u32>(GuestAddr(loc.used_gpa + 4 + 8 * slot + 4))
                .expect("can read used element length");
            assert_eq!(used_len, 1, "device wrote only the ack byte");

            *acc_mem.read::<u8>(GuestAddr(ack_gpa)).expect("can read ack byte")
        }

        fn ctrl_mac_table_set(
            &mut self,
            unicast: &[MacAddr],
            multicast: &[MacAddr],
        ) -> u8 {
            fn table(macs: &[MacAddr]) -> Vec<u8> {
                let mut buf = (macs.len() as u32).to_le_bytes().to_vec();
                for mac in macs {
                    buf.extend_from_slice(&mac.0);
                }
                buf
            }

            // Keep each table in its own descriptor.
            let unicast = table(unicast);
            let multicast = table(multicast);
            self.send_ctrl_cmd(
                1,
                control::MacCmd::TableSet as u8,
                &[&unicast, &multicast],
            )
        }
    }

    fn test_device_status_writes(test_ctx: TestCtx) -> TestCtx {
        // The device and driver collaborate via `device_status` to get the
        // device turned on. There's a subtlety here though, in VirtIO 1.2
        // section 2.1.2:
        //
        // > The driver MUST NOT clear a device status bit.
        //
        // which means if the device has set NEEDS_RESET, and a driver writes
        // back a status that would clear that bit, the driver is in violation.
        // Clearing any of the status bits will earn a warning and setting the
        // device status to NEEDS_RESET.

        let driver = test_ctx.create_driver();

        // First, if we just set up some bits and try to clear one, we won't
        // tolerate that..
        driver.write_status(Status::RESET);

        driver.set_status_bits(Status::ACK | Status::DRIVER);
        let mut status = driver.read_status();
        assert_eq!(status, Status::ACK | Status::DRIVER);

        status.remove(Status::DRIVER);
        driver.write_status(status);
        let status = driver.read_status();

        // No, no! If the guest has said they see the device and can drive it,
        // they can't decide to un-drive it anymore!
        assert!(status.contains(Status::NEEDS_RESET));

        // Okay, reset it and try again. This time we'll get it to NEEDS_RESET
        // "naturally"..
        driver.write_status(Status::RESET);

        driver.set_status_bits(Status::ACK | Status::DRIVER);

        let device_feats =
            driver.common_config.read_le32(common_cfg::device_feature);

        let features_u32: u32 = VIRTIO_NET_F_CTRL_VQ.try_into().unwrap();
        if device_feats & features_u32 == 0 {
            panic!("device does not support VIRTIO_NET_F_CTRL_VQ??");
        }

        driver
            .common_config
            .write_le32(common_cfg::driver_feature, features_u32);

        driver.set_status_bits(Status::FEATURES_OK);
        assert!(driver.read_status().contains(Status::FEATURES_OK));

        // Now write a bogus queue size. We'll set NEEDS_RESET for this.
        // VirtIO 1.2 says 32KiB is the max size. Further, we have not
        // negotiated VIRTIO_F_RING_PACKED, so the size must be a power of two.
        // Break both rules.
        driver.common_config.write_le16(common_cfg::queue_size, 65533);

        let mut status = driver.read_status();
        assert!(status.contains(Status::NEEDS_RESET));
        status.remove(Status::NEEDS_RESET);
        driver.write_status(status);

        // We should not be able to clear NEEDS_RESET without .. a reset.
        let real_status = driver.read_status();
        assert!(real_status.contains(Status::NEEDS_RESET));

        test_ctx
    }

    fn basic_operation_modern(test_ctx: TestCtx) -> TestCtx {
        let expected_feats =
            VIRTIO_NET_F_MAC | VIRTIO_NET_F_STATUS | VIRTIO_NET_F_CTRL_VQ;

        // Go through setting up the virtio NIC in a few scenarios, but don't
        // try using it or setting any interesting features.

        // First, we have a fresh device on a fresh VM. The test is playing the
        // role of the first use of the device by OVMF, an intiial bootloader,
        // or maybe the actual guest OS.
        let mut driver = test_ctx.create_driver();
        driver.modern_device_init(expected_feats);

        // Say we've done nothing with the device, but we've booted into
        // whatever next stage with its own driver that wants to operate the
        // device. It will go through 3.1.1 again.
        let mut driver = test_ctx.create_driver();
        driver.modern_device_init(expected_feats);

        // `Lifecycle::reset` is the kind of reset that occurs when a VM is
        // restarted. Do that now, as if the guest rebooted, triple faulted,
        // etc.
        Lifecycle::reset(test_ctx.dev.as_ref());

        // After a reset, reinit the device as if through OVMF->bootloader->OS
        // again..
        let mut driver = test_ctx.create_driver();
        driver.modern_device_init(expected_feats);
        let mut driver = test_ctx.create_driver();
        driver.modern_device_init(expected_feats);
        let mut driver = test_ctx.create_driver();
        driver.modern_device_init(expected_feats);

        // If the driver does not offer VIRTIO_NET_F_CTRL_VQ, then we
        // should clear the is_control flag from all queues.
        let mut driver = test_ctx.create_driver();
        driver.modern_device_init(VIRTIO_NET_F_MAC | VIRTIO_NET_F_STATUS);
        assert!(driver.ctl_qidx().is_none());

        test_ctx
    }

    fn basic_operation_multiqueue(test_ctx: TestCtx) -> TestCtx {
        // All the same operation as `basic_operation_modern`, but with
        // `VIRTIO_NET_F_MQ`.
        let expected_feats = VIRTIO_NET_F_MAC
            | VIRTIO_NET_F_STATUS
            | VIRTIO_NET_F_CTRL_VQ
            | VIRTIO_NET_F_MQ;

        let mut driver = test_ctx.create_driver();
        // OVMF just initializes all queues. Linux (at least 6.6.49/Alpine
        // 3.20.3) initializes all queues, then turns down the number of used
        // queues based on available CPUs, if this would be a limiter. Do
        // similar here to keep up the act.
        driver.set_max_pairs(Some(4));
        driver.modern_device_init(expected_feats);
        let mut driver = test_ctx.create_driver();
        driver.modern_device_init(expected_feats);
        Lifecycle::reset(test_ctx.dev.as_ref());
        let mut driver = test_ctx.create_driver();
        driver.modern_device_init(expected_feats);
        let mut driver = test_ctx.create_driver();
        driver.modern_device_init(expected_feats);
        // Pretending to be Linux, like above set_max_pairs().
        driver.set_max_pairs(Some(4));
        let mut driver = test_ctx.create_driver();
        driver.modern_device_init(expected_feats);

        test_ctx
    }

    /// Roughly approximation of a MQ-capable OS restarting, booting through
    /// with a simple single-queue driver, then booting back to a MQ-capable OS.
    fn multiqueue_to_singlequeue_to_multiqueue(test_ctx: TestCtx) -> TestCtx {
        // All the same operation as `basic_operation_modern`, but with
        // `VIRTIO_NET_F_MQ`.
        let expected_feats =
            VIRTIO_NET_F_MAC | VIRTIO_NET_F_STATUS | VIRTIO_NET_F_CTRL_VQ;

        let mut driver = test_ctx.create_driver();
        driver.modern_device_init(expected_feats | VIRTIO_NET_F_MQ);
        Lifecycle::reset(test_ctx.dev.as_ref());
        let mut driver = test_ctx.create_driver();
        driver.modern_device_init(expected_feats);
        let mut driver = test_ctx.create_driver();
        driver.modern_device_init(expected_feats | VIRTIO_NET_F_MQ);

        test_ctx
    }

    /// The same operations as `multiqueue_to_singlequeue_to_multiqueue`, above,
    /// but migrate the device between each operation.
    fn multiqueue_migration(test_ctx: TestCtx) -> TestCtx {
        // All the same operation as `basic_operation_modern`, but with
        // `VIRTIO_NET_F_MQ`.
        let expected_feats =
            VIRTIO_NET_F_MAC | VIRTIO_NET_F_STATUS | VIRTIO_NET_F_CTRL_VQ;

        let mut driver = test_ctx.create_driver();
        driver.modern_device_init(expected_feats | VIRTIO_NET_F_MQ);

        let drv_state = driver.export();
        let test_ctx = test_ctx.migrate();
        let driver = VirtioNetDriver::import(
            &test_ctx.machine,
            &test_ctx.dev,
            drv_state,
        );
        assert!(driver.status_ok());

        let mut driver = test_ctx.create_driver();
        // `basic_operation_multiqueue()` talks about why it's an interesting
        // test to shink max pairs.
        driver.set_max_pairs(Some(4));
        driver.modern_device_init(expected_feats | VIRTIO_NET_F_MQ);

        let drv_state = driver.export();
        let test_ctx = test_ctx.migrate();
        let driver = VirtioNetDriver::import(
            &test_ctx.machine,
            &test_ctx.dev,
            drv_state,
        );
        assert!(driver.status_ok());
        Lifecycle::reset(test_ctx.dev.as_ref());
        assert!(driver.status_ok());

        let drv_state = driver.export();
        let test_ctx = test_ctx.migrate();
        let mut driver = VirtioNetDriver::import(
            &test_ctx.machine,
            &test_ctx.dev,
            drv_state,
        );
        assert!(driver.status_ok());
        driver.modern_device_init(expected_feats);

        let drv_state = driver.export();
        let test_ctx = test_ctx.migrate();
        let mut driver = VirtioNetDriver::import(
            &test_ctx.machine,
            &test_ctx.dev,
            drv_state,
        );
        assert!(driver.status_ok());
        driver.modern_device_init(expected_feats | VIRTIO_NET_F_MQ);

        test_ctx
    }

    /// Go through the steps like an OVMF->Linux boot as described in tests
    /// above, but only migrate once we've reinitialized the NIC after enabling
    /// more queues than actually used at migration time.
    ///
    /// We once had a subtle bug here where the excess queues exported as
    /// enabled, but were below the `queues.len()` number of currently-enabled
    /// queues. Such queues imported (correctly!) on the other end as enabled,
    /// but were still "enabled" because reset did not cover them, and would
    /// make guests determine the device was simply broken. They were right!
    fn multiqueue_migration_after_boot(test_ctx: TestCtx) -> TestCtx {
        let expected_feats =
            VIRTIO_NET_F_MAC | VIRTIO_NET_F_STATUS | VIRTIO_NET_F_CTRL_VQ;

        let mut driver = test_ctx.create_driver();
        driver.modern_device_init(expected_feats | VIRTIO_NET_F_MQ);
        let mut driver = test_ctx.create_driver();
        // `basic_operation_multiqueue()` talks about why it's an interesting
        // test to shink max pairs.
        driver.set_max_pairs(Some(4));
        driver.modern_device_init(expected_feats | VIRTIO_NET_F_MQ);

        let drv_state = driver.export();
        let test_ctx = test_ctx.migrate();
        let driver = VirtioNetDriver::import(
            &test_ctx.machine,
            &test_ctx.dev,
            drv_state,
        );
        assert!(driver.status_ok());
        Lifecycle::reset(test_ctx.dev.as_ref());
        assert!(driver.status_ok());

        let mut driver = test_ctx.create_driver();
        driver.modern_device_init(expected_feats | VIRTIO_NET_F_MQ);
        let mut driver = test_ctx.create_driver();
        // Same as `set_max_pairs()` above.
        driver.set_max_pairs(Some(4));
        driver.modern_device_init(expected_feats | VIRTIO_NET_F_MQ);

        test_ctx
    }

    /// Exercise setting and reading back the guest MAC filter table through
    /// the control queue.
    fn mac_filters_install_read_and_clear(test_ctx: TestCtx) -> TestCtx {
        // The VLAN tag workaround pins promiscuity at construction.
        #[cfg(feature = "falcon")]
        if test_ctx.dev.inner.lock().unwrap().promisc == PromiscLevel::AllVlan {
            return test_ctx;
        }

        let mut driver = test_ctx.create_driver();
        driver.modern_device_init(
            VIRTIO_NET_F_MAC
                | VIRTIO_NET_F_STATUS
                | VIRTIO_NET_F_CTRL_VQ
                | VIRTIO_NET_F_CTRL_RX,
        );

        // All-multicast mode covers drivers that never manage a table.
        assert_eq!(
            test_ctx.dev.inner.lock().unwrap().promisc,
            PromiscLevel::AllMulti
        );
        assert!(test_ctx
            .dev
            .hdl
            .get_mac_filters()
            .expect("can read back filters")
            .is_empty());

        let all_hosts = MacAddr::from([0x01, 0x00, 0x5e, 0x00, 0x00, 0x01]);
        let all_nodes = MacAddr::from([0x33, 0x33, 0x00, 0x00, 0x00, 0x01]);
        let ack = driver.ctrl_mac_table_set(&[], &[all_hosts, all_nodes]);
        assert_eq!(ack, control::Ack::Ok as u8, "table install");

        {
            let state = test_ctx.dev.inner.lock().unwrap();
            assert_eq!(
                state
                    .multicast_mac_filters
                    .iter()
                    .copied()
                    .map(MacAddr::from)
                    .collect::<Vec<_>>(),
                vec![all_hosts, all_nodes]
            );
            assert!(state.mac_table_set);
            assert!(state.mac_filters_installed);
            assert_eq!(state.promisc, PromiscLevel::None);
        }

        assert_eq!(
            test_ctx.dev.hdl.get_mac_filters().expect("can read back filters"),
            vec![all_hosts, all_nodes],
        );

        // Duplicate and broadcast entries are dropped and the table is
        // compacted by the kernel.
        let bcast = MacAddr::from([0xff; ETHERADDRL]);
        let all_routers = MacAddr::from([0x33, 0x33, 0x00, 0x00, 0x00, 0x02]);
        let ack = driver.ctrl_mac_table_set(
            &[],
            &[all_hosts, bcast, all_hosts, all_routers],
        );
        assert_eq!(
            ack,
            control::Ack::Ok as u8,
            "table with broadcast and duplicate entries"
        );
        assert_eq!(
            test_ctx.dev.hdl.get_mac_filters().expect("can read back filters"),
            vec![all_hosts, all_routers],
        );

        let ack = driver.ctrl_mac_table_set(&[], &[]);
        assert_eq!(ack, control::Ack::Ok as u8, "table clear");
        {
            let state = test_ctx.dev.inner.lock().unwrap();
            assert!(!state.mac_filters_installed);
            assert_eq!(state.promisc, PromiscLevel::None);
        }
        assert!(test_ctx
            .dev
            .hdl
            .get_mac_filters()
            .expect("can read back filters")
            .is_empty());
        assert!(driver.status_ok());

        test_ctx
    }

    /// An oversized (well-formed) table is ack'ed, but the kernel should
    /// not install it. The device remains at all-multicast mode with no
    /// table installed on the MAC client.
    fn mac_filters_oversized_table(test_ctx: TestCtx) -> TestCtx {
        #[cfg(feature = "falcon")]
        if test_ctx.dev.inner.lock().unwrap().promisc == PromiscLevel::AllVlan {
            return test_ctx;
        }

        let mut driver = test_ctx.create_driver();
        driver.modern_device_init(
            VIRTIO_NET_F_MAC
                | VIRTIO_NET_F_STATUS
                | VIRTIO_NET_F_CTRL_VQ
                | VIRTIO_NET_F_CTRL_RX,
        );

        let capacity = discover_multicast_capacity(&test_ctx.dev.hdl);
        let all_hosts = MacAddr::from([0x01, 0x00, 0x5e, 0x00, 0x00, 0x01]);
        let ack = driver.ctrl_mac_table_set(&[], &[all_hosts]);
        assert_eq!(ack, control::Ack::Ok as u8);

        let oversized: Vec<MacAddr> = multicast_table(capacity + 1)
            .into_iter()
            .map(MacAddr::from)
            .collect();
        let ack = driver.ctrl_mac_table_set(&[], &oversized);
        assert_eq!(ack, control::Ack::Ok as u8);
        {
            let state = test_ctx.dev.inner.lock().unwrap();
            assert_eq!(state.multicast_mac_filters.len(), oversized.len());
            assert!(!state.mac_filters_installed);
            assert_eq!(state.promisc, PromiscLevel::AllMulti);
        }
        assert!(test_ctx
            .dev
            .hdl
            .get_mac_filters()
            .expect("can read back filters")
            .is_empty());
        assert!(driver.status_ok());

        test_ctx
    }

    /// A multicast table naming a unicast address is rejected (validation
    /// occurs before calling the kernel), leaving the previous table untouched.
    fn mac_filters_unicast_entry(test_ctx: TestCtx) -> TestCtx {
        #[cfg(feature = "falcon")]
        if test_ctx.dev.inner.lock().unwrap().promisc == PromiscLevel::AllVlan {
            return test_ctx;
        }

        let mut driver = test_ctx.create_driver();
        driver.modern_device_init(
            VIRTIO_NET_F_MAC
                | VIRTIO_NET_F_STATUS
                | VIRTIO_NET_F_CTRL_VQ
                | VIRTIO_NET_F_CTRL_RX,
        );

        let all_hosts = MacAddr::from([0x01, 0x00, 0x5e, 0x00, 0x00, 0x01]);
        let ack = driver.ctrl_mac_table_set(&[], &[all_hosts]);
        assert_eq!(ack, control::Ack::Ok as u8);

        let unicast = MacAddr::from([0x02, 0x08, 0x20, 0xac, 0x70, 0x99]);
        let ack = driver.ctrl_mac_table_set(&[], &[unicast]);
        assert_eq!(ack, control::Ack::Err as u8);

        {
            let state = test_ctx.dev.inner.lock().unwrap();
            assert_eq!(
                state
                    .multicast_mac_filters
                    .iter()
                    .copied()
                    .map(MacAddr::from)
                    .collect::<Vec<_>>(),
                vec![all_hosts]
            );
            assert_eq!(state.promisc, PromiscLevel::None);
        }
        assert_eq!(
            test_ctx.dev.hdl.get_mac_filters().expect("can read back filters"),
            vec![all_hosts],
        );
        assert!(driver.status_ok());

        test_ctx
    }

    /// The kernel [ioctl contract] rejects an oversized table before installing
    /// any of it.
    ///
    /// [ioctl contract]: https://github.com/oxidecomputer/illumos-gate/blob/5ffff4b86e486e1f9d7860be1368386699a7829a/usr/src/uts/intel/sys/viona_io.h#L184-L195
    fn mac_filters_oversized_table_ioctl(test_ctx: TestCtx) -> TestCtx {
        let hdl = &test_ctx.dev.hdl;

        let capacity = discover_multicast_capacity(hdl);
        let all_hosts = MacAddr::from([0x01, 0x00, 0x5e, 0x00, 0x00, 0x01]);
        hdl.set_mac_filters(&[all_hosts
            .try_into()
            .expect("baseline entry is multicast")])
            .expect("can install baseline filter");

        let oversized = multicast_table(capacity + 1);

        match hdl.set_mac_filters(&oversized) {
            Err(MacFilterError::Count { capacity: reported_capacity }) => {
                assert_eq!(reported_capacity as usize, capacity);
            }
            other => panic!("oversized table was not rejected: {other:?}"),
        }
        assert_eq!(
            hdl.get_mac_filters().expect("can read back filters"),
            vec![all_hosts],
        );

        test_ctx
    }

    fn mac_filters_reset_reapplies_promisc(test_ctx: TestCtx) -> TestCtx {
        use std::sync::atomic::Ordering::Relaxed;

        let mut driver = test_ctx.create_driver();
        let features = VIRTIO_NET_F_MAC
            | VIRTIO_NET_F_STATUS
            | VIRTIO_NET_F_CTRL_VQ
            | VIRTIO_NET_F_CTRL_RX;
        driver.modern_device_init(features);

        let promisc = test_ctx.dev.inner.lock().unwrap().promisc;
        test_ctx.dev.hdl.set_promisc(PromiscLevel::None).unwrap();
        let updates_before_reset = test_ctx.dev.hdl.1.load(Relaxed);
        {
            let state = test_ctx.dev.inner.lock().unwrap();
            assert_eq!(state.promisc, promisc);
            assert!(!state.rx_config_failed);
        }

        driver.modern_device_init(features);

        assert!(
            test_ctx.dev.hdl.1.load(Relaxed) > updates_before_reset,
            "reinitialization did not restore the promiscuous mode",
        );
        {
            let state = test_ctx.dev.inner.lock().unwrap();
            assert_eq!(state.promisc, promisc);
            assert!(!state.rx_config_failed);
        }
        assert!(driver.status_ok());

        test_ctx
    }

    fn mac_filters_reset_clears_kernel_state(test_ctx: TestCtx) -> TestCtx {
        #[cfg(feature = "falcon")]
        if test_ctx.dev.inner.lock().unwrap().promisc == PromiscLevel::AllVlan {
            return test_ctx;
        }

        let mut driver = test_ctx.create_driver();
        driver.modern_device_init(
            VIRTIO_NET_F_MAC
                | VIRTIO_NET_F_STATUS
                | VIRTIO_NET_F_CTRL_VQ
                | VIRTIO_NET_F_CTRL_RX,
        );

        let all_hosts = MacAddr::from([0x01, 0x00, 0x5e, 0x00, 0x00, 0x01]);
        assert_eq!(
            driver.ctrl_mac_table_set(&[], &[all_hosts]),
            control::Ack::Ok as u8
        );

        {
            let state = test_ctx.dev.inner.lock().unwrap();
            assert!(state.mac_filters_installed);
            assert_eq!(state.promisc, PromiscLevel::None);
        }
        assert_eq!(
            test_ctx.dev.hdl.get_mac_filters().expect("can read back filters"),
            vec![all_hosts],
        );

        driver.write_status(Status::RESET);

        // The status write leaves the kernel table stale until the next feature
        // negotiation, where `clear_guest_rx_state` removes it.
        assert_eq!(
            test_ctx.dev.hdl.get_mac_filters().expect("can read back filters"),
            vec![all_hosts],
        );

        driver.modern_device_init(
            VIRTIO_NET_F_MAC | VIRTIO_NET_F_STATUS | VIRTIO_NET_F_CTRL_VQ,
        );

        assert!(test_ctx
            .dev
            .hdl
            .get_mac_filters()
            .expect("can read back filters")
            .is_empty());
        {
            let state = test_ctx.dev.inner.lock().unwrap();
            assert!(!state.mac_filters_installed);
            assert!(!state.mac_table_set);
            assert_eq!(state.promisc, PromiscLevel::AllMulti);
        }
        assert!(driver.status_ok());

        test_ctx
    }

    fn mac_filters_migration_on_managed_table(test_ctx: TestCtx) -> TestCtx {
        #[cfg(feature = "falcon")]
        if test_ctx.dev.inner.lock().unwrap().promisc == PromiscLevel::AllVlan {
            return test_ctx;
        }

        let mut driver = test_ctx.create_driver();
        driver.modern_device_init(
            VIRTIO_NET_F_MAC
                | VIRTIO_NET_F_STATUS
                | VIRTIO_NET_F_CTRL_VQ
                | VIRTIO_NET_F_CTRL_RX,
        );

        let all_hosts = MacAddr::from([0x01, 0x00, 0x5e, 0x00, 0x00, 0x01]);
        assert_eq!(
            driver.ctrl_mac_table_set(&[], &[all_hosts]),
            control::Ack::Ok as u8
        );
        drop(driver);

        let test_ctx = test_ctx.migrate();

        {
            let state = test_ctx.dev.inner.lock().unwrap();
            assert!(state.mac_table_set);
            assert!(state.mac_filters_installed);
            assert_eq!(state.promisc, PromiscLevel::None);
        }
        assert_eq!(
            test_ctx.dev.hdl.get_mac_filters().expect("can read back filters"),
            vec![all_hosts],
        );

        test_ctx
    }

    fn mac_filters_migration_on_managed_empty_table(
        test_ctx: TestCtx,
    ) -> TestCtx {
        #[cfg(feature = "falcon")]
        if test_ctx.dev.inner.lock().unwrap().promisc == PromiscLevel::AllVlan {
            return test_ctx;
        }

        let mut driver = test_ctx.create_driver();
        driver.modern_device_init(
            VIRTIO_NET_F_MAC
                | VIRTIO_NET_F_STATUS
                | VIRTIO_NET_F_CTRL_VQ
                | VIRTIO_NET_F_CTRL_RX,
        );

        let all_hosts = MacAddr::from([0x01, 0x00, 0x5e, 0x00, 0x00, 0x01]);
        assert_eq!(
            driver.ctrl_mac_table_set(&[], &[all_hosts]),
            control::Ack::Ok as u8
        );
        assert_eq!(driver.ctrl_mac_table_set(&[], &[]), control::Ack::Ok as u8);
        drop(driver);

        let test_ctx = test_ctx.migrate();
        {
            let state = test_ctx.dev.inner.lock().unwrap();
            assert!(state.mac_table_set);
            assert!(!state.mac_filters_installed);
            assert_eq!(state.promisc, PromiscLevel::None);
        }
        assert!(test_ctx
            .dev
            .hdl
            .get_mac_filters()
            .expect("can read back filters")
            .is_empty());

        test_ctx
    }

    fn mac_filters_import_missing_payload(test_ctx: TestCtx) -> TestCtx {
        #[cfg(feature = "falcon")]
        if test_ctx.dev.inner.lock().unwrap().promisc == PromiscLevel::AllVlan {
            return test_ctx;
        }

        let viona_kind = <super::migrate::VionaStateV1 as Schema>::id().0;
        let payloads: Vec<_> = export_payloads(&test_ctx)
            .into_iter()
            .filter(|(kind, _, _)| kind.as_str() != viona_kind)
            .collect();

        let test_ctx = recreate_ctx(test_ctx);
        import_payloads(&test_ctx, &payloads)
            .expect("import without a viona payload succeeds");
        {
            let state = test_ctx.dev.inner.lock().unwrap();
            assert!(!state.mac_table_set);
            assert!(state.multicast_mac_filters.is_empty());
            assert_eq!(state.promisc, PromiscLevel::AllMulti);
        }

        test_ctx
    }

    fn mac_filters_import_invalid_rx_state(mut test_ctx: TestCtx) -> TestCtx {
        use super::migrate::VionaStateV1;

        type InvalidRxCase = (&'static str, fn(&mut VionaStateV1));

        #[cfg(feature = "falcon")]
        if test_ctx.dev.inner.lock().unwrap().promisc == PromiscLevel::AllVlan {
            return test_ctx;
        }

        let mut driver = test_ctx.create_driver();
        driver.modern_device_init(
            VIRTIO_NET_F_MAC
                | VIRTIO_NET_F_STATUS
                | VIRTIO_NET_F_CTRL_VQ
                | VIRTIO_NET_F_CTRL_RX,
        );
        let all_hosts = MacAddr::from([0x01, 0x00, 0x5e, 0x00, 0x00, 0x01]);
        assert_eq!(
            driver.ctrl_mac_table_set(&[], &[all_hosts]),
            control::Ack::Ok as u8
        );
        drop(driver);

        let payloads = export_payloads(&test_ctx);
        let viona_kind = VionaStateV1::id().0;
        let viona_payload = payloads
            .iter()
            .position(|(kind, _, _)| kind == viona_kind)
            .expect("export includes viona state");
        let cases: &[InvalidRxCase] = &[
            ("unicast entry", |state| {
                let unicast =
                    MacAddr::from([0x02, 0x08, 0x20, 0xac, 0x70, 0x99]);
                state.filter = super::FilterState::PROMISCUOUS.bits();
                state.unicast_mac_filters = vec![unicast];
                state.multicast_mac_filters = vec![unicast];
            }),
            ("unsupported RX_EXTRA flags", |state| {
                state.filter = super::FilterState::NO_MULTICAST.bits();
            }),
            ("source promiscuity was all-VLAN", |state| {
                state.promisc = super::migrate::PromiscMode::AllVlan;
            }),
        ];

        for &(expected_reason, modify) in cases {
            let mut payloads = payloads.clone();
            let mut input: VionaStateV1 =
                serde_json::from_str(&payloads[viona_payload].2)
                    .expect("can parse exported viona payload");
            modify(&mut input);
            payloads[viona_payload].2 = serde_json::to_string(&input)
                .expect("can reserialize viona payload");

            test_ctx = recreate_ctx(test_ctx);
            let err = import_payloads(&test_ctx, &payloads)
                .expect_err("invalid Rx state must fail import");
            let MigrateStateError::ImportFailed(reason) = err else {
                panic!("unexpected import error: {err:?}");
            };
            assert!(reason.contains(expected_reason), "{reason}");

            let state = test_ctx.dev.inner.lock().unwrap();
            assert!(state.filter.is_empty());
            assert!(state.unicast_mac_filters.is_empty());
            assert!(state.multicast_mac_filters.is_empty());
            assert!(!state.mac_table_set);
            assert!(!state.mac_filters_installed);
        }

        test_ctx
    }

    // Require construction with the Falcon all-VLAN mode.
    #[cfg(feature = "falcon")]
    fn mac_filters_host_promisc_pin(test_ctx: TestCtx) -> TestCtx {
        if test_ctx.dev.inner.lock().unwrap().promisc != PromiscLevel::AllVlan {
            return test_ctx;
        }
        let mut driver = test_ctx.create_driver();
        driver.modern_device_init(
            VIRTIO_NET_F_MAC
                | VIRTIO_NET_F_STATUS
                | VIRTIO_NET_F_CTRL_VQ
                | VIRTIO_NET_F_CTRL_RX,
        );

        let all_hosts = MacAddr::from([0x01, 0x00, 0x5e, 0x00, 0x00, 0x01]);
        assert_eq!(
            driver.ctrl_mac_table_set(&[], &[all_hosts]),
            control::Ack::Ok as u8
        );
        {
            let state = test_ctx.dev.inner.lock().unwrap();
            assert_eq!(state.promisc, PromiscLevel::AllVlan);
            assert!(state.mac_table_set);
            assert!(!state.mac_filters_installed);
        }
        assert!(test_ctx
            .dev
            .hdl
            .get_mac_filters()
            .expect("can read back filters")
            .is_empty());
        assert!(driver.status_ok());

        test_ctx
    }

    #[cfg(not(feature = "falcon"))]
    fn mac_filters_host_promisc_pin(test_ctx: TestCtx) -> TestCtx {
        test_ctx
    }

    // Bears an uncanny resemblance to `phd-test`...
    struct TestCase {
        name: &'static str,
        test_fn: fn(TestCtx) -> TestCtx,
    }

    fn create_vnic(phys_nic: &str, vnic_name: &str) {
        let res = Command::new("pfexec")
            .arg("dladm")
            .arg("create-vnic")
            .arg("-t")
            .arg("-l")
            .arg(phys_nic)
            .arg("-m")
            .arg("2:8:20:ac:70:0")
            .arg(vnic_name)
            .status()
            .expect("can create vnic");
        assert!(res.success());
    }

    fn delete_vnic(vnic_name: &str) {
        let res = Command::new("pfexec")
            .arg("dladm")
            .arg("delete-vnic")
            .arg(vnic_name)
            .status()
            .expect("can delete vnic");
        assert!(res.success());
    }

    // We'll actually create and destroy some vnics so not only do we need
    // `dladm`, we need a recent enough viona and everything.. this test is only
    // meaningful on an illumos host:;
    #[test]
    #[cfg_attr(not(target_os = "illumos"), ignore)]
    fn run_viona_tests() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();

        macro_rules! testcase {
            ($test_fn:ident) => {
                TestCase { name: stringify!($test_fn), test_fn: $test_fn }
            };
        }

        let tests = &[
            testcase!(test_device_status_writes),
            testcase!(basic_operation_modern),
            testcase!(basic_operation_multiqueue),
            testcase!(multiqueue_to_singlequeue_to_multiqueue),
            testcase!(multiqueue_migration),
            testcase!(multiqueue_migration_after_boot),
            testcase!(mac_filters_install_read_and_clear),
            testcase!(mac_filters_oversized_table),
            testcase!(mac_filters_unicast_entry),
            testcase!(mac_filters_oversized_table_ioctl),
            testcase!(mac_filters_reset_reapplies_promisc),
            testcase!(mac_filters_reset_clears_kernel_state),
            testcase!(mac_filters_migration_on_managed_table),
            testcase!(mac_filters_migration_on_managed_empty_table),
            testcase!(mac_filters_import_missing_payload),
            testcase!(mac_filters_import_invalid_rx_state),
            testcase!(mac_filters_host_promisc_pin),
        ];

        let underlying_nic = match std::env::var("VIONA_TEST_NIC") {
            Ok(val) => val,
            Err(VarError::NotPresent) => {
                eprintln!(
                    "Skipping viona tests as env does not have VIONA_TEST_NIC. \
                    Set this environment variable to an existing link that \
                    Propolis viona tests should create test vnics on.");
                let uname = nix::sys::utsname::uname().unwrap();
                if uname.machine() != std::ffi::OsStr::new("i86pc") {
                    // Since the tests are running on i86pc, this might be a dev
                    // host that does not actually want us messing with devices
                    // for tests.
                    //
                    // If the *tests* are running on a different architecture
                    // (say, "oxide"), assume that this is a misconfiguration
                    // instead and fail tests rather than "skip".
                    panic!(
                        "host ({}) is not i86pc, refusing to skip viona tests",
                        uname.machine().display()
                    );
                }
                return;
            }
            Err(VarError::NotUnicode(e)) => {
                panic!("non-unicode virtio host nic: {:?}", e.display());
            }
        };

        const TEST_VNIC: &'static str = "vnic_prop_test0";
        for test in tests {
            let underlying_nic = underlying_nic.clone();
            eprintln!("running viona test '{}'", test.name);
            rt.block_on(async move {
                create_vnic(&underlying_nic, TEST_VNIC);

                let res = std::panic::catch_unwind(move || {
                    let test_ctx =
                        create_test_ctx(test.name, &underlying_nic, TEST_VNIC);
                    Lifecycle::start(test_ctx.dev.as_ref())
                        .expect("can start viona device");
                    let test_ctx = (test.test_fn)(test_ctx);
                    drop(test_ctx);
                });
                delete_vnic(TEST_VNIC);
                if let Err(_) = res {
                    panic!("viona test '{}' was unsuccessful\n", test.name);
                } else {
                    eprintln!("viona test '{}' was successful\n", test.name);
                }
            });
        }
    }
}
