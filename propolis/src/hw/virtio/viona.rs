use std::fs::{File, OpenOptions};
use std::io::Result;
use std::os::unix::io::{AsRawFd, RawFd};
use std::sync::{Arc, Mutex, Weak};

use crate::common::*;
use crate::dispatch::events::{Event, EventTarget, FdEvents, Resource, Token};
use crate::dispatch::DispCtx;
use crate::hw::pci;
use crate::util::regmap::RegMap;
use crate::util::self_arc::*;
use crate::util::sys;
use crate::vmm::VmmHdl;

use super::bits::*;
use super::pci::PciVirtio;
use super::queue::VirtQueue;
use super::{VirtioDevice, VqChange, VqIntr};

use lazy_static::lazy_static;

const ETHERADDRL: usize = 6;

struct Inner {
    queues: Vec<Arc<VirtQueue>>,
    event_token: Option<Token>,
}
impl Inner {
    fn new() -> Self {
        Self { queues: Vec::new(), event_token: None }
    }
}

/// Represents a connection to the kernel's Viona (VirtIO Network Adapter)
/// driver.
pub struct VirtioViona {
    dev_features: u32,
    mac_addr: [u8; ETHERADDRL],
    mtu: u16,
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
                ro.write_u16(self.mtu);
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
        feat |= VIRTIO_NET_F_MTU;
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
                        if let VqIntr::MSI(a, d, masked) = intr.read() {
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

    fn device_reset(&self, ctx: &DispCtx) {
        let mut inner = self.inner.lock().unwrap();
        if inner.event_token.as_ref().is_none() {
            let token = ctx.event.fd_register(
                self.hdl.fd(),
                FdEvents::POLLRDBAND,
                self.self_weak() as Weak<dyn EventTarget>,
            );
            inner.event_token = Some(token);
        }
    }
}
impl EventTarget for VirtioViona {
    fn event_process(&self, event: &Event, ctx: &DispCtx) {
        match event.res {
            Resource::Fd(fd, _) => {
                assert_eq!(fd, self.hdl.fd());
                self.process_interrupts(ctx);
            }
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

mod bits {
    #![allow(unused)]

    pub const VIRTIO_NET_S_LINK_UP: u16 = 1 << 0;
    pub const VIRTIO_NET_S_ANNOUNCE: u16 = 1 << 1;

    pub const VIRTIO_NET_CFG_SIZE: usize = 0xc;
}
use bits::*;
