use std::fs::{File, OpenOptions};
use std::io::{Error, ErrorKind, Result};
use std::os::unix::io::{AsRawFd, RawFd};
use std::sync::{Arc, Mutex, Weak};

use crate::common::*;
use crate::dispatch::{AsyncCtx, AsyncTaskId, DispCtx};
use crate::hw::pci;
use crate::instance;
use crate::util::regmap::RegMap;
use crate::util::self_arc::*;
use crate::util::sys;
use crate::vmm::VmmHdl;

use super::bits::*;
use super::pci::PciVirtio;
use super::queue::VirtQueue;
use super::{VirtioDevice, VqChange, VqIntr};

use lazy_static::lazy_static;
use tokio::io::unix::AsyncFd;
use tokio::io::Interest;

const ETHERADDRL: usize = 6;

struct Inner {
    queues: Vec<Arc<VirtQueue>>,
    poller: Option<(Arc<VionaPoller>, AsyncTaskId)>,
}
impl Inner {
    fn new() -> Self {
        Self { queues: Vec::new(), poller: None }
    }
}

/// Represents a connection to the kernel's Viona (VirtIO Network Adapter)
/// driver.
pub struct VirtioViona {
    dev_features: u32,
    mac_addr: [u8; ETHERADDRL],
    mtu: Option<u16>,
    hdl: VionaHdl,
    inner: Mutex<Inner>,

    sa_cell: SelfArcCell<Self>,
}
impl VirtioViona {
    pub fn create(
        vnic_name: &str,
        queue_size: u16,
        vm: &VmmHdl,
    ) -> Result<Arc<pci::DeviceInst>> {
        let dlhdl = dladm::Handle::new()?;
        let info = dlhdl.query_vnic(vnic_name)?;
        let hdl = VionaHdl::new(info.link_id, vm.fd())?;

        let mut this = VirtioViona {
            dev_features: hdl.get_avail_features()?,
            mac_addr: [0; ETHERADDRL],
            mtu: info.mtu,
            hdl,
            inner: Mutex::new(Inner::new()),
            sa_cell: SelfArcCell::new(),
        };
        this.mac_addr.copy_from_slice(&info.mac_addr);
        drop(dlhdl);

        let mut this = Arc::new(this);
        SelfArc::self_arc_init(&mut this);

        // TX and RX
        let queue_count = 2;
        // interrupts for TX, RX, and device config
        let msix_count = Some(3);

        Ok(PciVirtio::create(
            queue_size,
            queue_count,
            msix_count,
            VIRTIO_DEV_NET,
            pci::bits::CLASS_NETWORK,
            VIRTIO_NET_CFG_SIZE,
            this,
        ))
    }

    fn process_interrupts(&self, ctx: &DispCtx) {
        let inner = self.inner.lock().unwrap();
        self.hdl
            .intr_poll(|vq_idx| {
                self.hdl.ring_intr_clear(vq_idx).unwrap();
                if let Some(vq) = inner.queues.get(vq_idx as usize) {
                    vq.with_intr(|intr| {
                        if let Some(intr) = intr {
                            intr.notify(ctx);
                        }
                    });
                }
            })
            .unwrap();
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
}
impl VirtioDevice for VirtioViona {
    fn device_cfg_rw(&self, mut rwo: RWOp) {
        NET_DEV_REGS.process(&mut rwo, |id, rwo| match rwo {
            RWOp::Read(ro) => self.net_cfg_read(id, ro),
            RWOp::Write(_) => {
                //ignore writes
            }
        });
    }
    fn device_get_features(&self) -> u32 {
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
    fn device_set_features(&self, feat: u32) {
        self.hdl
            .set_features(feat)
            .unwrap_or_else(|_| todo!("viona error handling"));
    }

    fn queue_notify(&self, vq: &Arc<VirtQueue>, _ctx: &DispCtx) {
        self.hdl
            .ring_kick(vq.id)
            .unwrap_or_else(|_| todo!("viona error handling"));
    }
    fn queue_change(
        &self,
        vq: &Arc<VirtQueue>,
        change: VqChange,
        _ctx: &DispCtx,
    ) {
        match change {
            VqChange::Reset => {
                self.hdl
                    .ring_reset(vq.id)
                    .unwrap_or_else(|_| todo!("viona error handling"));
            }
            VqChange::Address => {
                if let Some(info) = vq.map_info() {
                    self.hdl
                        .ring_init(vq.id, vq.size, info.desc_addr)
                        .unwrap_or_else(|_| todo!("viona error handling"));
                }
            }
            VqChange::IntrCfg => {
                let mut addr = 0;
                let mut msg = 0;
                vq.with_intr(|i| {
                    if let Some(intr) = i {
                        if let VqIntr::Msi(a, d, masked) = intr.read() {
                            // If the entry (or entire MSI function) is masked,
                            // keep the in-kernel MSI acceleration disabled with
                            // the zeroed address and message.  That will allow
                            // us to poll the queue interrupt state and expose
                            // it, as expected, via the PBA.
                            if !masked {
                                addr = a;
                                msg = d;
                            }
                        }
                    }
                });
                self.hdl
                    .ring_cfg_msi(vq.id, addr, msg)
                    .unwrap_or_else(|_| todo!("viona error handling"));
            }
        }
    }

    fn attach(&self, queues: &[Arc<VirtQueue>]) {
        let mut inner = self.inner.lock().unwrap();
        // Keep references to all of the virtqueues around so we can issue
        // interrupts to them.  This is necessary when MSI is not configured or
        // is masked (device-wide or for a given queue).
        for vq in queues {
            inner.queues.push(Arc::clone(vq));
        }
    }
}
impl Entity for VirtioViona {
    fn state_transition(
        &self,
        next: instance::State,
        target: Option<instance::State>,
        ctx: &DispCtx,
    ) {
        match next {
            instance::State::Quiesce => {
                // XXX: This is a dirty hack, but we need to stop the viona
                // rings from running in order to reset or halt the instance.
                assert!(matches!(
                    target,
                    Some(instance::State::Reset) | Some(instance::State::Halt)
                ));
                let mut inner = self.inner.lock().unwrap();
                let (poller, task) = inner.poller.take().unwrap();
                ctx.cancel_async(task);
                drop(poller);
                for vq in inner.queues.iter() {
                    let _ = self.hdl.ring_reset(vq.id);
                }
            }
            instance::State::Boot => {
                // Get interrupt notification for the rings setup
                let (poller, task) =
                    VionaPoller::spawn(self.hdl.fd(), self.self_weak(), ctx)
                        .unwrap();
                let mut inner = self.inner.lock().unwrap();
                assert!(inner.poller.is_none());
                inner.poller = Some((poller, task));
            }
            _ => {}
        }
    }
}
impl SelfArc for VirtioViona {
    fn self_arc_cell(&self) -> &SelfArcCell<Self> {
        &self.sa_cell
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

struct VionaHdl {
    fp: File,
}
impl VionaHdl {
    fn new(link_id: u32, vm_fd: RawFd) -> Result<Self> {
        let fp = OpenOptions::new()
            .read(true)
            .write(true)
            .open(viona_api::VIONA_DEV_PATH)?;

        let mut vna_create =
            viona_api::vioc_create { c_linkid: link_id, c_vmfd: vm_fd };
        sys::ioctl(fp.as_raw_fd(), viona_api::VNA_IOC_CREATE, &mut vna_create)?;
        Ok(Self { fp })
    }
    fn fd(&self) -> RawFd {
        self.fp.as_raw_fd()
    }
    fn get_avail_features(&self) -> Result<u32> {
        let mut value = 0;
        sys::ioctl(self.fd(), viona_api::VNA_IOC_GET_FEATURES, &mut value)?;
        Ok(value)
    }
    fn set_features(&self, feat: u32) -> Result<()> {
        let mut value = feat;
        sys::ioctl(self.fd(), viona_api::VNA_IOC_SET_FEATURES, &mut value)?;
        Ok(())
    }
    fn ring_init(&self, idx: u16, size: u16, addr: u64) -> Result<()> {
        let mut vna_ring_init = viona_api::vioc_ring_init {
            ri_index: idx,
            ri_qsize: size,
            _pad: [0; 2],
            ri_qaddr: addr,
        };
        sys::ioctl(
            self.fd(),
            viona_api::VNA_IOC_RING_INIT,
            &mut vna_ring_init,
        )?;
        Ok(())
    }
    fn ring_reset(&self, idx: u16) -> Result<()> {
        sys::ioctl_usize(
            self.fd(),
            viona_api::VNA_IOC_RING_RESET,
            idx as usize,
        )?;
        Ok(())
    }
    fn ring_kick(&self, idx: u16) -> Result<()> {
        sys::ioctl_usize(
            self.fd(),
            viona_api::VNA_IOC_RING_KICK,
            idx as usize,
        )?;
        Ok(())
    }
    fn ring_cfg_msi(&self, idx: u16, addr: u64, msg: u32) -> Result<()> {
        let mut vna_ring_msi = viona_api::vioc_ring_msi {
            rm_index: idx,
            _pad: [0; 3],
            rm_addr: addr,
            rm_msg: msg as u64,
        };
        sys::ioctl(
            self.fd(),
            viona_api::VNA_IOC_RING_SET_MSI,
            &mut vna_ring_msi,
        )?;
        Ok(())
    }
    fn intr_poll(&self, mut f: impl FnMut(u16)) -> Result<()> {
        let mut vna_ip = viona_api::vioc_intr_poll {
            vip_status: [0; viona_api::VIONA_VQ_MAX as usize],
        };
        sys::ioctl(self.fd(), viona_api::VNA_IOC_INTR_POLL, &mut vna_ip)?;
        for i in 0..viona_api::VIONA_VQ_MAX {
            if vna_ip.vip_status[i as usize] != 0 {
                f(i)
            }
        }
        Ok(())
    }
    fn ring_intr_clear(&self, idx: u16) -> Result<()> {
        sys::ioctl_usize(
            self.fd(),
            viona_api::VNA_IOC_RING_INTR_CLR,
            idx as usize,
        )?;
        Ok(())
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
struct VionaPoller {
    epfd: RawFd,
    dev: Weak<VirtioViona>,
}
impl VionaPoller {
    fn spawn(
        viona_fd: RawFd,
        dev: Weak<VirtioViona>,
        ctx: &DispCtx,
    ) -> Result<(Arc<Self>, AsyncTaskId)> {
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
        let this = Arc::new(Self { epfd, dev });
        let for_spawn = Arc::clone(&this);
        let task = ctx.spawn_async(move |mut actx| {
            Box::pin(async move {
                let _ = for_spawn.poll_interrupts(&mut actx).await;
            })
        });

        Ok((this, task))
    }
    fn event_present(&self) -> Result<bool> {
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
    async fn poll_interrupts(&self, actx: &mut AsyncCtx) {
        let afd =
            AsyncFd::with_interest(self.epfd, Interest::READABLE).unwrap();
        loop {
            let readable = afd.readable().await;
            if readable.is_err() {
                return;
            }
            match self.event_present() {
                Ok(false) => {
                    continue;
                }
                Ok(true) => {}
                Err(_) => {
                    return;
                }
            };

            if let Some(ctx) = actx.dispctx().await {
                let dev = Weak::upgrade(&self.dev).unwrap();
                dev.process_interrupts(&ctx);
            } else {
                return;
            }
        }
    }
}
impl Drop for VionaPoller {
    fn drop(&mut self) {
        unsafe {
            libc::close(self.epfd);
        }
    }
}

mod bits {
    #![allow(unused)]

    pub const VIRTIO_NET_S_LINK_UP: u16 = 1 << 0;
    pub const VIRTIO_NET_S_ANNOUNCE: u16 = 1 << 1;

    pub const VIRTIO_NET_CFG_SIZE: usize = 0xc;
}
use bits::*;
