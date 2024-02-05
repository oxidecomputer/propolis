// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

#![cfg_attr(not(target_os = "illumos"), allow(dead_code, unused_imports))]

use std::io::{self, Error, ErrorKind};
use std::num::NonZeroU16;
use std::os::unix::io::{AsRawFd, RawFd};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Condvar, Mutex, Weak};

use crate::common::*;
use crate::hw::pci;
use crate::migrate::*;
use crate::util::regmap::RegMap;
use crate::vmm::VmmHdl;

use super::bits::*;
use super::pci::{PciVirtio, PciVirtioState};
use super::queue::{self, VirtQueue, VirtQueues};
use super::{VirtioDevice, VqChange, VqIntr};

use lazy_static::lazy_static;
use tokio::io::unix::AsyncFd;
use tokio::io::Interest;
use tokio::sync::watch;
use tokio::task::JoinHandle;

const ETHERADDRL: usize = 6;

/// Viona's in-kernel emulation of the device VirtQueues is performed in what it
/// calls "vrings".  Since the userspace portion of the Viona emulation is
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
    vring_state: [VRingState; 2],
}
impl Inner {
    fn new() -> Self {
        Self { poller: None, vring_state: [Default::default(); 2] }
    }

    /// Get the `VRingState` for a given VirtQueue
    fn for_vq(&mut self, vq: &VirtQueue) -> &mut VRingState {
        let id = vq.id as usize;
        assert!(id < self.vring_state.len());

        &mut self.vring_state[id]
    }
}

/// Represents a connection to the kernel's Viona (VirtIO Network Adapter)
/// driver.
pub struct PciVirtioViona {
    virtio_state: PciVirtioState,
    pci_state: pci::DeviceState,

    dev_features: u32,
    mac_addr: [u8; ETHERADDRL],
    mtu: Option<u16>,
    hdl: VionaHdl,
    inner: Mutex<Inner>,
}
impl PciVirtioViona {
    pub fn new(
        vnic_name: &str,
        queue_size: u16,
        vm: &VmmHdl,
    ) -> io::Result<Arc<PciVirtioViona>> {
        let dlhdl = dladm::Handle::new()?;
        let info = dlhdl.query_vnic(vnic_name)?;
        let hdl = VionaHdl::new(info.link_id, vm.fd())?;

        // TX and RX
        let queue_count = NonZeroU16::new(2).unwrap();
        // interrupts for TX, RX, and device config
        let msix_count = Some(3);
        let dev_features = hdl.get_avail_features()?;

        let queues =
            VirtQueues::new(NonZeroU16::new(queue_size).unwrap(), queue_count);
        let (virtio_state, pci_state) = PciVirtioState::create(
            queues,
            msix_count,
            VIRTIO_DEV_NET,
            VIRTIO_SUB_DEV_NET,
            pci::bits::CLASS_NETWORK,
            VIRTIO_NET_CFG_SIZE,
        );

        let mut this = PciVirtioViona {
            virtio_state,
            pci_state,

            dev_features,
            mac_addr: [0; ETHERADDRL],
            mtu: info.mtu,
            hdl,
            inner: Mutex::new(Inner::new()),
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

    fn process_interrupts(&self) {
        if let Some(mem) = self.pci_state.acc_mem.access() {
            self.hdl
                .intr_poll(|vq_idx| {
                    self.hdl.ring_intr_clear(vq_idx).unwrap();
                    self.virtio_state.queues[vq_idx as usize].send_intr(&mem);
                })
                .unwrap();
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
                // hard-wired to single vq pair for now
                ro.write_u16(1);
            }
            NetReg::Mtu => {
                // Guests should not be asking for this value unless
                // VIRTIO_NET_F_MTU has been set. However, we'd rather lie
                // (return zero) than unwrap and panic here.
                ro.write_u16(self.mtu.unwrap_or(0));
            }
        }
    }

    /// Pause the associated virtqueues and sync any in-kernel state for them
    /// into the userspace representation.
    fn queues_sync(&self) {
        let mut inner = self.inner.lock().unwrap();
        for vq in self.virtio_state.queues.iter() {
            if !vq.live.load(Ordering::Acquire) {
                continue;
            }

            let rs = inner.for_vq(vq);
            match *rs {
                VRingState::Ready | VRingState::Run | VRingState::Paused => {
                    // Ensure the ring is paused for a consistent snapshot
                    if *rs != VRingState::Paused {
                        if self.hdl.ring_pause(vq.id).is_err() {
                            *rs = VRingState::Error;
                            continue;
                        }
                        *rs = VRingState::Paused;
                    }

                    if let Ok(live) = self.hdl.ring_get_state(vq.id) {
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
            if self.hdl.ring_reset(vq.id).is_err() {
                *rs = VRingState::Fatal;
                res = Err(());
                // Although this fatal vring state means the device itself will
                // require a reset (which itself is unlikely to work), we
                // continue attempting to reset/restart the other VQs.
                continue;
            }

            *rs = VRingState::Init;
            let info = vq.get_state();
            if info.mapping.valid {
                if self.hdl.ring_set_state(vq.id, vq.size, &info).is_err() {
                    *rs = VRingState::Error;
                    continue;
                }

                if let Some(intr_cfg) = vq.read_intr() {
                    if self.hdl.ring_cfg_msi(vq.id, Some(intr_cfg)).is_err() {
                        *rs = VRingState::Error;
                        continue;
                    }
                }
                *rs = VRingState::Ready;

                if vq.live.load(Ordering::Acquire) {
                    // If the ring was already running, kick it.
                    if self.hdl.ring_kick(vq.id).is_err() {
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
        let mut inner = self.inner.lock().unwrap();
        for vq in self.virtio_state.queues.iter() {
            let rs = inner.for_vq(vq);
            match *rs {
                VRingState::Init => {
                    // Already at rest
                }
                VRingState::Fatal => {
                    // No sense in attempting a reset
                }
                _ => {
                    if self.hdl.ring_reset(vq.id).is_err() {
                        *rs = VRingState::Fatal;
                    } else {
                        *rs = VRingState::Init;
                    }
                }
            }
        }
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
}
impl VirtioDevice for PciVirtioViona {
    fn cfg_rw(&self, mut rwo: RWOp) {
        NET_DEV_REGS.process(&mut rwo, |id, rwo| match rwo {
            RWOp::Read(ro) => self.net_cfg_read(id, ro),
            RWOp::Write(_) => {
                //ignore writes
            }
        });
    }
    fn get_features(&self) -> u32 {
        let mut feat = VIRTIO_NET_F_MAC;
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
    fn set_features(&self, feat: u32) -> Result<(), ()> {
        self.hdl.set_features(feat).map_err(|_| ())
    }

    fn queue_notify(&self, vq: &Arc<VirtQueue>) {
        let mut inner = self.inner.lock().unwrap();
        let rs = inner.for_vq(vq);
        match rs {
            VRingState::Ready | VRingState::Run => {
                if self.hdl.ring_kick(vq.id).is_err() {
                    *rs = VRingState::Error;
                } else {
                    *rs = VRingState::Run;
                }
            }
            _ => {}
        }
    }
    fn queue_change(
        &self,
        vq: &Arc<VirtQueue>,
        change: VqChange,
    ) -> Result<(), ()> {
        let mut inner = self.inner.lock().unwrap();
        let rs = inner.for_vq(vq);

        match change {
            VqChange::Reset => {
                if self.hdl.ring_reset(vq.id).is_err() {
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
                        if self.hdl.ring_reset(vq.id).is_err() {
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
                let info = vq.get_state();
                if !info.mapping.valid {
                    return Ok(());
                }

                if self
                    .hdl
                    .ring_init(vq.id, vq.size, info.mapping.desc_addr)
                    .is_err()
                {
                    // Bad virtqueue configuration is not fatal.  While the
                    // vring will not transition to running, we will be content
                    // to wait for the guest to later provide a valid config.
                    *rs = VRingState::Error;
                    return Ok(());
                }

                if let Some(intr_cfg) = vq.read_intr() {
                    if self.hdl.ring_cfg_msi(vq.id, Some(intr_cfg)).is_err() {
                        *rs = VRingState::Error;
                    }
                }
                *rs = VRingState::Ready;
            }
            VqChange::IntrCfg => {
                if *rs != VRingState::Fatal {
                    if self.hdl.ring_cfg_msi(vq.id, vq.read_intr()).is_err() {
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
    }
    fn start(&self) -> anyhow::Result<()> {
        // This device initializes into a paused state. Starting it is
        // equivalent to resuming it.
        self.resume();
        Ok(())
    }
    fn pause(&self) {
        self.poller_stop(false);
        self.queues_sync();
    }
    fn resume(&self) {
        self.poller_start();
        if self.queues_restart().is_err() {
            self.virtio_state.set_needs_reset(self);
        }
    }
    fn halt(&self) {
        self.poller_stop(true);
        // Destroy any in-kernel state to prevent it from impeding instance
        // destruction.
        self.queues_kill();
        let _ = self.hdl.delete();
    }
    fn migrate(&self) -> Migrator {
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
        <dyn PciVirtio>::import(self, offer, ctx)
    }
}

#[derive(Copy, Clone, Eq, PartialEq, Debug)]
enum NetReg {
    Mac,
    Status,
    MaxVqPairs,
    Mtu,
}
lazy_static! {
    static ref NET_DEV_REGS: RegMap<NetReg> = {
        let layout = [
            (NetReg::Mac, 6),
            (NetReg::Status, 2),
            (NetReg::MaxVqPairs, 2),
            (NetReg::Mtu, 2),
        ];
        RegMap::create_packed(VIRTIO_NET_CFG_SIZE, &layout, None)
    };
}

use viona_api::VionaFd;

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
    fn get_avail_features(&self) -> io::Result<u32> {
        let mut value = 0;
        unsafe {
            self.0.ioctl(viona_api::VNA_IOC_GET_FEATURES, &mut value)?;
        }
        Ok(value)
    }
    fn set_features(&self, feat: u32) -> io::Result<()> {
        let mut value = feat;
        unsafe {
            self.0.ioctl(viona_api::VNA_IOC_SET_FEATURES, &mut value)?;
        }
        Ok(())
    }
    fn ring_init(&self, idx: u16, size: u16, addr: u64) -> io::Result<()> {
        let mut vna_ring_init = viona_api::vioc_ring_init {
            ri_index: idx,
            ri_qsize: size,
            _pad: [0; 2],
            ri_qaddr: addr,
        };
        unsafe {
            self.0.ioctl(viona_api::VNA_IOC_RING_INIT, &mut vna_ring_init)?;
        }
        Ok(())
    }
    fn ring_reset(&self, idx: u16) -> io::Result<()> {
        self.0.ioctl_usize(viona_api::VNA_IOC_RING_RESET, idx as usize)?;
        Ok(())
    }
    fn ring_kick(&self, idx: u16) -> io::Result<()> {
        self.0.ioctl_usize(viona_api::VNA_IOC_RING_KICK, idx as usize)?;
        Ok(())
    }
    fn ring_pause(&self, idx: u16) -> io::Result<()> {
        self.0.ioctl_usize(viona_api::VNA_IOC_RING_PAUSE, idx as usize)?;
        Ok(())
    }
    fn ring_set_state(
        &self,
        idx: u16,
        size: u16,
        info: &queue::Info,
    ) -> io::Result<()> {
        let mut cfg = viona_api::vioc_ring_state {
            vrs_index: idx,
            vrs_avail_idx: info.avail_idx,
            vrs_used_idx: info.used_idx,
            vrs_qsize: size,
            vrs_qaddr: info.mapping.desc_addr,
        };
        unsafe {
            self.0.ioctl(viona_api::VNA_IOC_RING_SET_STATE, &mut cfg)?;
        }
        Ok(())
    }
    fn ring_get_state(&self, idx: u16) -> io::Result<queue::Info> {
        let mut cfg =
            viona_api::vioc_ring_state { vrs_index: idx, ..Default::default() };
        unsafe {
            self.0.ioctl(viona_api::VNA_IOC_RING_GET_STATE, &mut cfg)?;
        }
        Ok(queue::Info {
            mapping: queue::MapInfo {
                desc_addr: cfg.vrs_qaddr,
                avail_addr: 0,
                used_addr: 0,
                valid: true,
            },
            avail_idx: cfg.vrs_avail_idx,
            used_idx: cfg.vrs_used_idx,
        })
    }
    fn ring_cfg_msi(&self, idx: u16, cfg: Option<VqIntr>) -> io::Result<()> {
        let (addr, msg) = match cfg {
            Some(VqIntr::Msi(a, m, masked)) if !masked => (a, m),
            // If MSI is disabled, or the entry is masked (individually,
            // or at the function level), then disable in-kernel
            // acceleration of MSI delivery.
            _ => (0, 0),
        };

        let mut vna_ring_msi = viona_api::vioc_ring_msi {
            rm_index: idx,
            _pad: [0; 3],
            rm_addr: addr,
            rm_msg: msg as u64,
        };
        unsafe {
            self.0.ioctl(viona_api::VNA_IOC_RING_SET_MSI, &mut vna_ring_msi)?;
        }
        Ok(())
    }
    fn intr_poll(&self, mut f: impl FnMut(u16)) -> io::Result<()> {
        let mut vna_ip = viona_api::vioc_intr_poll {
            vip_status: [0; viona_api::VIONA_VQ_MAX as usize],
        };
        unsafe {
            self.0.ioctl(viona_api::VNA_IOC_INTR_POLL, &mut vna_ip)?;
        }
        for i in 0..viona_api::VIONA_VQ_MAX {
            if vna_ip.vip_status[i as usize] != 0 {
                f(i)
            }
        }
        Ok(())
    }
    fn ring_intr_clear(&self, idx: u16) -> io::Result<()> {
        self.0.ioctl_usize(viona_api::VNA_IOC_RING_INTR_CLR, idx as usize)?;
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

    pub const VIRTIO_NET_CFG_SIZE: usize = 0xc;
}
use bits::*;

/// Check that available viona API matches expectations of propolis crate
pub(crate) fn check_api_version() -> Result<(), crate::api_version::Error> {
    let fd = viona_api::VionaFd::open()?;
    let vers = fd.api_version()?;

    // viona only requires the V2 bits for now
    let compare = viona_api::ApiVersion::V2;

    if vers < compare {
        Err(crate::api_version::Error::Mismatch("viona", vers, compare as u32))
    } else {
        Ok(())
    }
}
