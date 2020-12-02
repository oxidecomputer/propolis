use std::io::Result;
use std::sync::Arc;

use crate::block::*;
use crate::common::*;
use crate::dispatch::DispCtx;
use crate::hw::pci;
use crate::util::regmap::RegMap;

use super::pci::PciVirtio;
use super::queue::{Chain, VirtQueue};
use super::VirtioDevice;

use byteorder::{ByteOrder, LE};
use dladm;
use lazy_static::lazy_static;

const VIRTIO_DEV_NET: u16 = 0x1000;

const VIRTIO_NET_F_CSUM: u32 = 1 << 0;
const VIRTIO_NET_F_GUEST_CSUM: u32 = 1 << 1;
const VIRTIO_NET_F_CTRL_GUEST_OFFLOADS: u32 = 1 << 2;
const VIRTIO_NET_F_MTU: u32 = 1 << 3;
const VIRTIO_NET_F_MAC: u32 = 1 << 5;
const VIRTIO_NET_F_GUEST_TSO4: u32 = 1 << 7;
const VIRTIO_NET_F_GUEST_TSO6: u32 = 1 << 8;
const VIRTIO_NET_F_GUEST_ECN: u32 = 1 << 9;
const VIRTIO_NET_F_GUEST_UFO: u32 = 1 << 10;
const VIRTIO_NET_F_HOST_TSO4: u32 = 1 << 11;
const VIRTIO_NET_F_HOST_TSO6: u32 = 1 << 12;
const VIRTIO_NET_F_HOST_ECN: u32 = 1 << 13;
const VIRTIO_NET_F_HOST_UFO: u32 = 1 << 14;
const VIRTIO_NET_F_MGR_RXBUF: u32 = 1 << 15;
const VIRTIO_NET_F_STATUS: u32 = 1 << 16;
const VIRTIO_NET_F_CTRL_VQ: u32 = 1 << 17;
const VIRTIO_NET_F_CTRL_RX: u32 = 1 << 18;
const VIRTIO_NET_F_CTRL_VLAN: u32 = 1 << 19;

const VIRTIO_NET_S_LINK_UP: u16 = 1 << 0;
const VIRTIO_NET_S_ANNOUNCE: u16 = 1 << 1;

const VIRTIO_NET_CFG_SIZE: usize = 0xc;

const ETHERADDRL: usize = 6;

pub struct VirtioViona {
    dev_features: u32,
    link_id: u32,
    mac_addr: [u8; ETHERADDRL],
    mtu: u16,
}
impl VirtioViona {
    pub fn new(
        vnic_name: &str,
        queue_size: u16,
    ) -> Result<Arc<pci::DeviceInst>> {
        let dlhdl = dladm::Handle::new()?;
        let info = dlhdl.query_vnic(vnic_name)?;
        let mut inner = VirtioViona {
            dev_features: 0,
            link_id: info.link_id,
            mac_addr: [0; ETHERADDRL],
            mtu: info.mtu,
        };
        inner.mac_addr.copy_from_slice(&info.mac_addr);
        drop(dlhdl);

        // TX and RX
        let queue_count = 2;
        // interrupts for TX, RX, and device config
        let msix_count = Some(3);

        Ok(PciVirtio::new(
            queue_size,
            queue_count,
            msix_count,
            VIRTIO_DEV_NET,
            pci::bits::CLASS_NETWORK,
            VIRTIO_NET_CFG_SIZE,
            Box::new(inner),
        ))
    }

    fn net_cfg_read(&self, id: &NetReg, ro: &mut ReadOp) {
        match id {
            NetReg::Mac => {
                ro.buf.copy_from_slice(&self.mac_addr)
            }
            NetReg::Status => {
                // Always report link up
                LE::write_u16(ro.buf, VIRTIO_NET_S_LINK_UP);
            }
            NetReg::MaxVqPairs => {
                // hard-wired to single vq pair for now
                LE::write_u16(ro.buf, 1);
            }
            NetReg::Mtu => {
                LE::write_u16(ro.buf, self.mtu);
            }
        }
    }
}
impl VirtioDevice for VirtioViona {
    fn device_cfg_rw(&self, rwo: &mut RWOp) {
        NET_DEV_REGS.process(rwo, |id, rwo| match rwo {
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
        // todo!("communicate features to viona device");
    }

    fn queue_notify(&self, qid: u16, vq: &Arc<VirtQueue>, ctx: &DispCtx) {
        // todo!("notification trigger");
        if qid == 1 {
            // pretend to transmit for now, discarding the packets
            let mem = &ctx.mctx.memctx();

            loop {
                let mut chain = Chain::with_capacity(4);
                let clen = vq.pop_avail(&mut chain, mem);
                if clen.is_none() {
                    break;
                }
                vq.push_used(&mut chain, mem, ctx);
            }
        }
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
