// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

#![cfg_attr(not(target_os = "illumos"), allow(dead_code, unused_imports))]

use std::io::{self, Error, ErrorKind};
use std::num::NonZeroU16;
use std::os::unix::io::{AsRawFd, RawFd};
use std::sync::{Arc, Condvar, Mutex, Weak};
use std::sync::atomic::{Ordering, AtomicBool};

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
use tokio::io::unix::AsyncFd;
use tokio::io::Interest;
use tokio::sync::watch;
use tokio::task::JoinHandle;

// Re-export API versioning interface for convenience of propolis consumers
pub use viona_api::{api_version, ApiVersion};

pub const RX_QUEUE_SIZE: VqSize = VqSize::new(0x800);
pub const TX_QUEUE_SIZE: VqSize = VqSize::new(0x100);
pub const CTL_QUEUE_SIZE: VqSize = VqSize::new(32);

pub const VIRTIO_MQ_MIN_QPAIRS: u16 = 1;
pub const VIRTIO_MQ_MAX_QPAIRS: u16 = 0x8000;

pub const PROPOLIS_MAX_MQ_PAIRS: u16 = 11;

pub const fn max_num_queues() -> usize {
    PROPOLIS_MAX_MQ_PAIRS as usize * 2
}

const ETHERADDRL: usize = 6;

/// The caller of `set_use_pairs` will probably be inlined into a larger
/// function that is difficult to spot in a ustack(). This gives us a hint
/// about why we were `set_usepairs()`'ing.
#[repr(u8)]
enum MqSetPairsCause {
    Reset = 0,
    MqEnabled = 1,
    Commanded = 2,
}

#[usdt::provider(provider = "propolis")]
mod probes {
    fn virtio_viona_mq_set_use_pairs(cause: u8, npairs: u16) {}
}

/// Types and so forth for supporting the control queue.
/// Note that these come from the VirtIO spec, section
/// 5.1.6.2 in VirtIO 1.2.
pub mod control {
    use super::ETHERADDRL;
    use std::convert::TryFrom;

    /// The control message header has two data: a u8 representing the "class"
    /// of control message, which describes what the message applies to, and a
    /// "command", which describes what action we should take in response to the
    /// command. So for example, class Mq and command Set means to set the
    /// number of multiqueue queue pairs.
    #[derive(Clone, Copy, Debug, Default)]
    #[repr(C)]
    pub struct Header {
        class: u8,
        command: u8,
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

    #[derive(Clone, Copy, Debug)]
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

    #[derive(Clone, Copy, Debug, strum::FromRepr)]
    #[repr(u8)]
    pub enum MacCmd {
        TableSet = 0,
        AddrSet = 1,
    }

    #[derive(Clone, Copy, Debug, Default)]
    #[repr(C)]
    pub struct Mac {
        entries: u32,
        mac: [u8; ETHERADDRL],
    }

    #[derive(Clone, Copy, Debug, Default)]
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
}

/// Viona's in-kernel emulation of the device VirtQueues is performed in what
/// are calls "vrings". Since the userspace portion of the Viona emulation is
/// tasked with keeping the vring state in sync with the VirtQueue it
/// represents, we must track its perceived state.
#[derive(Copy, Clone, Default, Eq, PartialEq)]
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

struct Inner {
    poller: Option<PollerHdl>,
    iop_state: Option<NonZeroU16>,
    notify_mmio_addr: Option<u64>,
    vring_state: Vec<VRingState>,
}
impl Inner {
    fn new(max_queues: usize) -> Self {
        let vring_state = vec![Default::default(); max_queues];
        let poller = None;
        let iop_state = None;
        let notify_mmio_addr = None;
        Self { poller, iop_state, notify_mmio_addr, vring_state }
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

/// Represents a connection to the kernel's Viona (VirtIO Network Adapter)
/// driver.
pub struct PciVirtioViona {
    virtio_state: PciVirtioState,
    pci_state: pci::DeviceState,
    indicator: lifecycle::Indicator,

    dev_features: u64,
    mac_addr: [u8; ETHERADDRL],
    mtu: Option<u16>,
    hdl: VionaHdl,
    mq_active: AtomicBool,
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

        #[cfg(feature = "falcon")]
        if let Err(e) = hdl.set_promisc(viona_api::VIONA_PROMISC_ALL_VLAN) {
            // Until/unless this support is integrated into stlouis/illumos,
            // this is an expected failure.   This is needed to use vlans,
            // but shouldn't affect any other use case.
            eprintln!("failed to enable promisc mode on {vnic_name}: {e:?}");
        }

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
        // until the driver negotiates multiqueue, we only use the first two.
        let queues = VirtQueues::new_with_len(3, &queue_sizes);
        if let Some(ctlq) = queues.get(2) {
            ctlq.set_control();
        }
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
        let mut this = PciVirtioViona {
            virtio_state,
            pci_state,
            indicator: Default::default(),
            dev_features,
            mac_addr: [0; ETHERADDRL],
            mtu: info.mtu,
            mq_active: AtomicBool::new(false),
            hdl,
            inner: Mutex::new(Inner::new(nqueues)),
        };
        this.mac_addr.copy_from_slice(&info.mac_addr);
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
                .intr_poll(self.virtio_state.queues.len() - 1, |vq_idx| {
                    self.hdl.ring_intr_clear(vq_idx).unwrap();
                    let vq = self.virtio_state.queues.get(vq_idx).unwrap();
                    vq.send_intr(&mem);
                })
                .unwrap();
        }
    }

    fn is_ctl_queue(&self, vq: &VirtQueue) -> bool {
        usize::from(vq.id) + 1 == self.virtio_state.queues.len()
    }

    fn ctl_queue_notify(&self, vq: &VirtQueue) {
        if let Some(mem) = self.pci_state.acc_mem.access() {
            while !vq.avail_is_empty(&mem) {
                let mut chain = Chain::with_capacity(4);
                let intrs_en = vq.disable_intr(&mem);
                while let Some((_idx, _len)) = vq.pop_avail(&mut chain, &mem) {
                    let res = match self.ctl_msg(vq, &mut chain, &mem) {
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

    fn ctl_msg(
        &self,
        vq: &VirtQueue,
        chain: &mut Chain,
        mem: &MemCtx,
    ) -> Result<(), ()> {
        let mut header = control::Header::default();
        if !chain.read(&mut header, &mem) {
            return Err(());
        }
        use control::Command;
        match Command::try_from(header).map_err(|_| ())? {
            Command::Rx(cmd) => self.ctl_rx(cmd, vq, chain, mem),
            Command::Mac(cmd) => self.ctl_mac(cmd, vq, chain, mem),
            Command::Vlan(_) => Ok(()),
            Command::Announce(_) => Ok(()),
            Command::Mq(cmd) => self.ctl_mq(cmd, vq, chain, mem),
        }
    }

    fn ctl_rx(
        &self,
        cmd: control::RxCmd,
        vq: &VirtQueue,
        chain: &mut Chain,
        mem: &MemCtx,
    ) -> Result<(), ()> {
        let _todo = (cmd, vq, chain, mem);
        Err(())
    }

    fn ctl_mac(
        &self,
        cmd: control::MacCmd,
        vq: &VirtQueue,
        chain: &mut Chain,
        mem: &MemCtx,
    ) -> Result<(), ()> {
        let _todo = (cmd, vq, chain, mem);
        Err(())
    }

    fn set_use_pairs(&self, requested: u16) -> Result<(), ()> {
        if requested < 1 || PROPOLIS_MAX_MQ_PAIRS < requested {
            return Err(());
        }
        let npairs = requested as usize;
        if npairs == self.virtio_state.queues.len() {
            return Ok(());
        }
        self.hdl.set_usepairs(requested).unwrap();
        self.virtio_state
            .queues
            .set_len(npairs * 2 + 1)
            .expect("num queue pairs");
        Ok(())
    }

    fn ctl_mq(
        &self,
        cmd: control::MqCmd,
        vq: &VirtQueue,
        chain: &mut Chain,
        mem: &MemCtx,
    ) -> Result<(), ()> {
        use control::MqCmd;
        let _todo = vq;
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
            NetReg::Mac => ro.write_bytes(&self.mac_addr),
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
        let _ = poller.sender.send(TargetState::Run);
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
        wait_state.wait_stopped();
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
}
impl VirtioDevice for PciVirtioViona {
    fn rw_dev_config(&self, mut rwo: RWOp) {
        NET_DEV_REGS.process(&mut rwo, |id, rwo| match rwo {
            RWOp::Read(ro) => self.net_cfg_read(id, ro),
            RWOp::Write(_) => {
                //ignore writes
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
        let want_mq = feat & VIRTIO_NET_F_MQ != 0;

        if want_mq {
            // This might be the first we're hearing from the guest about
            // wanting multi-queue. If it is, we'll have a bit of work to do.
            let mq_active = self.mq_active.swap(true, Ordering::Relaxed);

            if !mq_active {
                self.hdl.set_pairs(PROPOLIS_MAX_MQ_PAIRS).map_err(|_| ())?;
                probes::virtio_viona_mq_set_use_pairs!(|| (
                    MqSetPairsCause::MqEnabled as u8,
                    PROPOLIS_MAX_MQ_PAIRS
                ));
                self.set_use_pairs(PROPOLIS_MAX_MQ_PAIRS)?;
            }
        }
        Ok(())
    }

    fn queue_notify(&self, vq: &VirtQueue) {
        if self.is_ctl_queue(vq) {
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
        self.mq_active.store(false, Ordering::Relaxed);
        self.virtio_state.queues.reset_peak();
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
        <dyn PciVirtio>::export(self, output, ctx)
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

        Ok(())
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
        viona_api::vioc_ring_state {
            vrs_index: id,
            vrs_qsize: size,
            vrs_qaddr_desc: desc_addr,
            vrs_qaddr_avail: avail_addr,
            vrs_qaddr_used: used_addr,
            ..Default::default()
        }
    }
}

struct VionaHdl(VionaFd);
impl VionaHdl {
    fn new(link_id: u32, vm_fd: RawFd) -> io::Result<Self> {
        let vfd = VionaFd::new(link_id, vm_fd)?;

        Ok(Self(vfd))
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
            unsafe {
                self.0.ioctl(
                    viona_api::VNA_IOC_RING_INIT_MODERN,
                    &mut vna_ring_init,
                )?;
            }
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

    ///
    /// Set the desired promiscuity level on this interface.
    #[cfg(feature = "falcon")]
    fn set_promisc(&self, p: i32) -> io::Result<()> {
        self.0.ioctl_usize(viona_api::VNA_IOC_SET_PROMISC, p as usize)?;
        Ok(())
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
    fn set_stopped(&self) {
        let mut guard = self.running.lock().unwrap();
        if *guard {
            *guard = false;
            self.cv.notify_all();
        }
    }
    fn set_running(&self) {
        let mut guard = self.running.lock().unwrap();
        *guard = true;
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

/// Check that available viona API matches expectations of propolis crate
pub(crate) fn check_api_version() -> Result<(), crate::api_version::Error> {
    let vers = viona_api::api_version()?;

    // when setting up a vNIC, Propolis will unconditionally do the SET_PAIRS
    // ioctl, which requires V6.
    let want = viona_api::ApiVersion::V6 as u32;

    if vers < want {
        Err(crate::api_version::Error::TooLow { have: vers, want })
    } else {
        Ok(())
    }
}
