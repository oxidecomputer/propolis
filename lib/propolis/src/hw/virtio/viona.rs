// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

#![cfg_attr(not(target_os = "illumos"), allow(dead_code, unused_imports))]

use std::io::{self, Error, ErrorKind};
use std::num::NonZeroU16;
use std::os::unix::io::{AsRawFd, RawFd};
use std::sync::{Arc, Condvar, Mutex, Weak};

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
    Import = 3,
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
        if (feat & VIRTIO_NET_F_MQ) != 0 {
            self.hdl.set_pairs(PROPOLIS_MAX_MQ_PAIRS).map_err(|_| ())?;
            probes::virtio_viona_mq_set_use_pairs!(|| (
                MqSetPairsCause::MqEnabled as u8,
                PROPOLIS_MAX_MQ_PAIRS
            ));
            self.set_use_pairs(PROPOLIS_MAX_MQ_PAIRS)?;
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

        if (feat & VIRTIO_NET_F_MQ) != 0 {
            self.hdl.set_pairs(PROPOLIS_MAX_MQ_PAIRS).unwrap();
        }
        let queues = self.virtio_state.queues.count().get();
        let Some(pairs) = (queues - 1).checked_div(2) else {
            return Err(MigrateStateError::ImportFailed(format!(
                "source queue count was not a number of pairs + 1: {queues}"
            )));
        };
        probes::virtio_viona_mq_set_use_pairs!(|| (
            MqSetPairsCause::Import as u8,
            pairs
        ));
        self.hdl.set_usepairs(pairs).map_err(|e| {
            MigrateStateError::ImportFailed(format!(
                "error while restoring use pairs ({pairs}): {e:?}"
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
        VIRTIO_NET_F_CTRL_VQ, VIRTIO_NET_F_MAC, VIRTIO_NET_F_MQ,
        VIRTIO_NET_F_STATUS,
    };
    use crate::hw::virtio::PciVirtioViona;
    use crate::lifecycle::Lifecycle;
    use crate::migrate::{
        MigrateCtx, MigrateMulti, PayloadOffer, PayloadOffers, PayloadOutputs,
    };
    use crate::Machine;
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
            let mut dev_payloads = PayloadOutputs::new();
            let acc_mem =
                self.machine.acc_mem.access().expect("machine has memory");
            let ctx = MigrateCtx { mem: &acc_mem };
            <PciVirtioViona>::export(&self.dev, &mut dev_payloads, &ctx)
                .expect("can export PciVirtioViona");
            let mut payloads = Vec::new();
            for output in dev_payloads.into_iter() {
                let bytes = serde_json::to_string(&output.payload)
                    .expect("serializing payload output");
                let serialized = (output.kind, output.version, bytes);
                payloads.push(serialized);
            }

            // Loosely follow the structure of `import_device` as `propolis-server` would; the
            // combination of type erasure and borrows make this somewhat more complicated than it
            // would ideally be..
            let mut desers = Vec::new();
            for (_, _, bytes) in payloads.iter() {
                desers.push(serde_json::Deserializer::from_str(&bytes));
            }
            let mut offers = Vec::new();
            for ((kind, version, _bytes), deser) in
                payloads.iter().zip(desers.iter_mut())
            {
                let deserialized =
                    Box::new(<dyn erased_serde::Deserializer>::erase(deser));
                offers.push(PayloadOffer {
                    kind,
                    version: *version,
                    payload: deserialized,
                });
            }
            let mut offers = PayloadOffers::new(offers);

            let vnic_name = self.vnic_name.clone();
            let underlying_nic = self.underlying_nic.clone();
            let test_name = self.test_name;

            std::mem::drop(acc_mem);
            std::mem::drop(self);

            delete_vnic(&vnic_name);
            create_vnic(&underlying_nic, &vnic_name);

            let new_ctx =
                create_test_ctx(test_name, &underlying_nic, &vnic_name);
            let acc_mem = new_ctx
                .machine
                .acc_mem
                .access()
                .expect("new machine has memory");
            let new_migrate = MigrateCtx { mem: &acc_mem };
            <PciVirtioViona>::import(&new_ctx.dev, &mut offers, &new_migrate)
                .expect("can import PciVirtioViona");
            Lifecycle::start(new_ctx.dev.as_ref())
                .expect("can start viona device");
            new_ctx
        }
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
    }

    impl DriverState {
        fn new() -> Self {
            Self {
                max_pairs: None,
                // Start virtio-nic queues somewhere other than address 0.
                next_queue_gpa: 2 * MB as u64,
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

        // Modern and legacy queue layout requirements differ a bit, but this
        // sets up queues in the legacy format to be usable in both contexts.
        //
        // This does not actually initialize the descriptor tables in any
        // meaningful way! These queues are not actually usable!
        fn init_queue(&mut self, queue: u16) {
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

            let descriptor_table_gpa = self.state.next_queue_gpa;
            self.common_config
                .write_le64(common_cfg::queue_desc, descriptor_table_gpa);

            let avail_gpa = descriptor_table_gpa.next_multiple_of(page_u64);
            // First, flags.
            // > If the VIRTIO_F_EVENT_IDX feature bit is not negotiated:
            // > * The driver MUST set flags to 0 or 1.
            // > * The driver MAY set flags to 1 to advise the device that
            //     notifications are not needed.
            acc_mem.write::<u32>(GuestAddr(avail_gpa), &0);
            // Index. "This starts at 0, and increases."
            acc_mem.write::<u32>(GuestAddr(avail_gpa + 4), &0);
            // Leave all the `ring` entries uninitialized, and we've not
            // negotiated VIRTIO_F_EVENT_IDX so no `used_event` for now.
            self.common_config.write_le64(common_cfg::queue_driver, avail_gpa);

            let used_gpa = avail_gpa.next_multiple_of(page_u64);

            self.common_config.write_le64(common_cfg::queue_device, used_gpa);

            self.common_config.write_le16(common_cfg::queue_enable, 1);

            // Finally, round up so the next queue (if there is one) starts
            // page-aligned like it should.
            self.state.next_queue_gpa = used_gpa.next_multiple_of(page_u64);

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

            // TODO:
            // if `features` includes multi-queue, for example, a guest might check
            // the number of supported queues to decide if it actually can operate
            // the device in multi-queue mode...
            //
            // We know that `features` is a subset of `device_feats` by
            // `unsupported` being zero, above.
            eprintln!("writing features: {:#08x}", features_u32);
            self.common_config
                .write_le32(common_cfg::driver_feature, features_u32);

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

            let n_qpairs = if features & VIRTIO_NET_F_MQ == 0 {
                1
            } else {
                // We'll configure all of the device's queues here. This is what
                // we've seen both Linux and Windows do with virtio devices (in
                // Linux, virtnet_probe()->init_vqs()). The number of
                // actually-used queues is only configured later.
                let max_pairs = self
                    .device_config
                    .read_le16(net_config::max_virtqueue_pairs);
                // TODO: the *right* thing to do here would be to set up the control
                // queue, send an MQ command with VQ_PAIRS_SET, then wait for a
                // (should be immediate) response. Or .. just call into the device
                // directly.
                self.dev
                    .set_use_pairs(max_pairs)
                    .expect("can set_use_pairs(max_pairs)");
                max_pairs
            };
            let n_queues = n_qpairs * 2;
            eprintln!("n_qpairs: {}", n_qpairs);

            for queue in 0..n_queues {
                eprintln!("initializing queue {}", queue);
                self.init_queue(queue);
                assert!(self.status_ok());
            }

            if n_qpairs > 1 {
                // Again following in the footsteps of observed Windows/Linux
                // virtio drivers: now that queues are all initialized, set the
                // number of queue pairs we'll actually use. The test (playing
                // the role of the guest OS) may have selected less than the
                // maximum queue pairs.
                let wanted_pairs = self.state.max_pairs.unwrap_or(n_qpairs);
                assert!(wanted_pairs <= n_qpairs);
                self.dev
                    .set_use_pairs(wanted_pairs)
                    .expect("can set_use_pairs(wanted_pairs)");
            }

            if features & VIRTIO_NET_F_CTRL_VQ != 0 {
                // configure the control queue too?
                self.common_config
                    .write_le16(common_cfg::queue_select, n_queues);
            }

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
        // OVMF just initializes all queues. Linux (6at least 6.6.49/Alpine
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
            rt.block_on(async move {
                create_vnic(&underlying_nic, TEST_VNIC);

                let test_ctx =
                    create_test_ctx(test.name, &underlying_nic, TEST_VNIC);
                Lifecycle::start(test_ctx.dev.as_ref())
                    .expect("can start viona device");
                let test_ctx = (test.test_fn)(test_ctx);
                drop(test_ctx);

                delete_vnic(TEST_VNIC);
            });
        }
    }
}
