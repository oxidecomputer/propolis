use std::mem;
use std::num::Wrapping;
use std::sync::{Arc, Mutex, Weak};

use crate::pci;
use crate::types::*;
use crate::util::regmap::RegMap;

use byteorder::{ByteOrder, LE};
use lazy_static::lazy_static;

const VIRTIO_VENDOR: u16 = 0x1af4;
const VIRTIO_DEV_BLOCK: u16 = 0x1001;

const VIRTIO_F_NOTIFY_ON_EMPTY: u32 = 1 << 24;
const VIRTIO_F_RING_INDIRECT_DESC: u32 = 1 << 28;
const VIRTIO_F_RING_EVENT_IDX: u32 = 1 << 29;

bitflags! {
    #[derive(Default)]
    pub struct Status: u8 {
        const RESET = 0;
        const ACK = 1;
        const DRIVER = 2;
        const DRIVER_OK = 4;
        const FEATURES_OK = 8;
        const NEEDS_RESET = 64;
        const FAILED = 128;
    }
}

struct VirtioState {
    status: Status,
    queue_sel: u16,
    nego_feat: u32,
    isr_status: u8,
}
impl VirtioState {
    fn new() -> Self {
        Self {
            status: Status::RESET,
            queue_sel: 0,
            nego_feat: 0,
            isr_status: 0,
        }
    }
}

pub struct PciVirtio<D: VirtioDevice> {
    dev: D,
    map: RegMap<VirtioTop>,
    state: Mutex<VirtioState>,
    queue_size: u16,
    num_queues: u16,
    queues: Vec<Mutex<VirtQueue>>,
}
impl<D: VirtioDevice> PciVirtio<D> {
    fn map_for_device() -> RegMap<VirtioTop> {
        let dev_sz = D::device_cfg_size();
        // XXX: Shortened for no-msix
        let legacy_sz = 0x16;
        let layout = [
            (VirtioTop::LegacyConfig, legacy_sz),
            (VirtioTop::DeviceConfig, dev_sz),
        ];
        let size = dev_sz + legacy_sz;
        RegMap::create_packed_passthru(size, &layout)
    }

    fn new(
        queue_size: u16,
        num_queues: u16,
        inner: D,
    ) -> Arc<pci::DeviceInst<Self>> {
        assert!(queue_size > 1 && queue_size.is_power_of_two());

        let mut queues = Vec::new();
        for _ in 0..num_queues {
            queues.push(Mutex::new(VirtQueue::new(queue_size)));
        }

        let this = Self {
            dev: inner,
            map: Self::map_for_device(),
            state: Mutex::new(VirtioState::new()),
            queue_size,
            num_queues,
            queues,
        };
        let (id, class) = D::device_id_and_class();
        pci::Builder::new(pci::Ident {
            vendor_id: VIRTIO_VENDOR,
            device_id: id,
            sub_vendor_id: VIRTIO_VENDOR,
            sub_device_id: id - 0xfff,
            class,
            ..Default::default()
        })
        .add_bar_io(pci::BarN::BAR0, 0x200)
        .add_lintr()
        .finish(this)
    }

    fn legacy_read(&self, id: &LegacyReg, ro: &mut ReadOp) {
        match id {
            LegacyReg::FeatDevice => {
                LE::write_u32(ro.buf, self.features_supported());
            }
            LegacyReg::FeatDriver => {
                let state = self.state.lock().unwrap();
                LE::write_u32(ro.buf, state.nego_feat);
            }
            LegacyReg::QueueAddr => {
                let state = self.state.lock().unwrap();
                if state.queue_sel < self.num_queues {
                    let ql = &self.queues[state.queue_sel as usize];
                    let addr = ql.lock().unwrap().gpa_desc.0;
                    LE::write_u32(ro.buf, addr as u32);
                } else {
                    // bogus queue
                    LE::write_u32(ro.buf, 0);
                }
            }
            LegacyReg::QueueSize => {
                LE::write_u16(ro.buf, self.queue_size);
            }
            LegacyReg::QueueSelect => {
                let state = self.state.lock().unwrap();
                LE::write_u16(ro.buf, state.queue_sel);
            }
            LegacyReg::QueueNotify => {}
            LegacyReg::DeviceStatus => {
                let state = self.state.lock().unwrap();
                ro.buf[0] = state.status.bits();
            }
            LegacyReg::IsrStatus => {
                let mut state = self.state.lock().unwrap();
                ro.buf[0] = state.isr_status;
                state.isr_status = 0;
            }
            _ => {
                // no msix for now
            }
        }
    }
    fn legacy_write(&self, id: &LegacyReg, wo: &WriteOp) {
        match id {
            LegacyReg::FeatDriver => {
                let nego = LE::read_u32(wo.buf) & self.features_supported();
                let mut state = self.state.lock().unwrap();
                state.nego_feat = nego;
                self.dev.device_set_features(nego);
            }
            LegacyReg::QueueAddr => {
                let mut state = self.state.lock().unwrap();
                let mut success = false;
                if state.queue_sel < self.num_queues {
                    let ql = &self.queues[state.queue_sel as usize];
                    let mut queue = ql.lock().unwrap();
                    success = queue.map_legacy(LE::read_u32(wo.buf));
                }
                if !success {
                    // XXX: interrupt needed?
                    state.status |= Status::FAILED;
                }
            }
            LegacyReg::QueueSelect => {
                let mut state = self.state.lock().unwrap();
                state.queue_sel = LE::read_u16(wo.buf);
            }
            LegacyReg::QueueNotify => {
                self.queue_notify(LE::read_u16(wo.buf));
            }
            LegacyReg::DeviceStatus => {
                self.set_status(wo.buf[0]);
            }

            LegacyReg::FeatDevice
            | LegacyReg::QueueSize
            | LegacyReg::IsrStatus => {
                // Read-only regs
            }
            LegacyReg::MsixVectorConfig | LegacyReg::MsixVectorQueue => {
                unimplemented!("no msix for now")
            }
        }
    }
    fn reg_read(&self, id: &VirtioTop, ro: &mut ReadOp) {
        match id {
            VirtioTop::LegacyConfig => {
                LEGACY_REGS.read(ro, |id, ro| self.legacy_read(id, ro))
            }
            VirtioTop::DeviceConfig => self.dev.device_cfg_read(ro),
        }
    }
    fn reg_write(&self, id: &VirtioTop, wo: &WriteOp) {
        match id {
            VirtioTop::LegacyConfig => LEGACY_REGS.write(
                wo,
                |id, wo| self.legacy_write(id, wo),
                |id, ro| self.legacy_read(id, ro),
            ),
            VirtioTop::DeviceConfig => self.dev.device_cfg_write(wo),
        }
    }

    fn features_supported(&self) -> u32 {
        self.dev.device_get_features() | VIRTIO_F_RING_INDIRECT_DESC
    }
    fn set_status(&self, status: u8) {
        let mut state = self.state.lock().unwrap();
        // XXX: do more?
        let val = Status::from_bits_truncate(status);
        state.status = val;
    }
    fn queue_notify(&self, queue: u16) {
        // XXX: do more
        println!("notify virtqueue {}", queue);
    }
}
impl<D: VirtioDevice> pci::Device for PciVirtio<D> {
    fn bar_read(&self, bar: pci::BarN, ro: &mut ReadOp) {
        assert_eq!(bar, pci::BarN::BAR0);
        self.map.read(ro, |id, ro| self.reg_read(id, ro));
    }

    fn bar_write(&self, bar: pci::BarN, wo: &WriteOp) {
        assert_eq!(bar, pci::BarN::BAR0);
        self.map.write(
            wo,
            |id, wo| self.reg_write(id, wo),
            |id, ro| self.reg_read(id, ro),
        );
    }
}

pub trait VirtioDevice: Send + Sync {
    fn device_cfg_size() -> usize;
    fn device_cfg_read(&self, ro: &mut ReadOp);
    fn device_cfg_write(&self, wo: &WriteOp);
    fn device_get_features(&self) -> u32;
    fn device_set_features(&self, feat: u32);
    fn device_id_and_class() -> (u16, u8);
}

#[repr(C)]
struct VqDesc {
    addr: u64,
    len: u32,
    flags: u16,
    next: u16,
}
#[repr(C)]
struct VqUsed {
    id: u32,
    len: u32,
}

enum VqStatus {
    Init,
    Mapped,
    Error,
}

struct VirtQueue {
    size: u16,
    gpa_desc: GuestAddr,
    gpa_avail: GuestAddr,
    gpa_used: GuestAddr,
    cur_avail_idx: Wrapping<u16>,
    used_idx: Wrapping<u16>,
    status: VqStatus,
}
const LEGACY_QALIGN: u64 = 0x1000;
fn qalign(addr: u64, align: u64) -> u64 {
    let mask = align - 1;
    (addr + mask) & !mask
}
impl VirtQueue {
    fn new(size: u16) -> Self {
        assert!(size.is_power_of_two());
        Self {
            size,
            gpa_desc: GuestAddr(0),
            gpa_avail: GuestAddr(0),
            gpa_used: GuestAddr(0),
            cur_avail_idx: Wrapping(0),
            used_idx: Wrapping(0),
            status: VqStatus::Init,
        }
    }
    fn reset(&mut self) {
        self.status = VqStatus::Init;
        self.gpa_desc = GuestAddr(0);
        self.gpa_avail = GuestAddr(0);
        self.gpa_used = GuestAddr(0);
        self.cur_avail_idx = Wrapping(0);
        self.used_idx = Wrapping(0);
    }
    fn map_legacy(&mut self, addr: u32) -> bool {
        // even if the map is unsuccessful, track the address provided
        self.gpa_desc = GuestAddr(addr as u64);

        if (addr & !(LEGACY_QALIGN as u32)) != 0 {
            self.status = VqStatus::Error;
            return false;
        }
        let size = self.size as usize;

        let desc_addr = addr as u64;
        let desc_len = mem::size_of::<VqDesc>() * size;
        let avail_addr = desc_addr + desc_len as u64;
        let avail_len = mem::size_of::<u16>() * (size + 3);
        let used_addr = qalign(avail_addr, LEGACY_QALIGN) + avail_len as u64;
        let used_len =
            mem::size_of::<VqUsed>() * size + mem::size_of::<u16>() * 3;

        if used_addr + used_len as u64 > u32::MAX as u64 {
            self.status = VqStatus::Error;
            return false;
        }
        self.gpa_avail = GuestAddr(avail_addr);
        self.gpa_used = GuestAddr(used_addr);
        self.cur_avail_idx = Wrapping(0);
        self.used_idx = Wrapping(0);
        self.status = VqStatus::Mapped;
        true
    }
}

#[derive(Copy, Clone, Eq, PartialEq, Debug)]
enum VirtioTop {
    LegacyConfig,
    DeviceConfig,
}

#[derive(Copy, Clone, Eq, PartialEq, Debug)]
enum LegacyReg {
    FeatDevice,
    FeatDriver,
    QueueAddr,
    QueueSize,
    QueueSelect,
    QueueNotify,
    DeviceStatus,
    IsrStatus,
    MsixVectorConfig,
    MsixVectorQueue,
}
lazy_static! {
    static ref LEGACY_REGS: RegMap<LegacyReg> = {
        let layout = [
            (LegacyReg::FeatDevice, 4),
            (LegacyReg::FeatDriver, 4),
            (LegacyReg::QueueAddr, 4),
            (LegacyReg::QueueSize, 2),
            (LegacyReg::QueueSelect, 2),
            (LegacyReg::QueueNotify, 2),
            (LegacyReg::DeviceStatus, 1),
            (LegacyReg::IsrStatus, 1),
            (LegacyReg::MsixVectorConfig, 2),
            (LegacyReg::MsixVectorQueue, 2),
        ];
        let size = 0x18;
        RegMap::create_packed(size, &layout, None)
    };
}

pub struct VirtioBlock {}
impl VirtioBlock {
    pub fn new(queue_size: u16) -> Arc<pci::DeviceInst<PciVirtio<Self>>> {
        PciVirtio::new(queue_size, 1, Self {})
    }

    fn block_cfg_read(&self, id: &BlockReg, ro: &mut ReadOp) {
        match id {
            BlockReg::Capacity => {}
            BlockReg::SizeMax => {}
            BlockReg::SegMax => {}
            BlockReg::GeoCyl => {}
            BlockReg::GeoHeads => {}
            BlockReg::GeoSectors => {}
            BlockReg::BlockSize => {}
            BlockReg::TopoPhysExp => {}
            BlockReg::TopoAlignOff => {}
            BlockReg::TopoMinIoSz => {}
            BlockReg::TopoOptIoSz => {}
            BlockReg::Writeback => {}
            BlockReg::MaxDiscardSectors => {}
            BlockReg::MaxDiscardSeg => {}
            BlockReg::DiscardSectorAlign => {}
            BlockReg::MaxZeroSectors => {}
            BlockReg::MaxZeroSeg => {}
            BlockReg::ZeroMayUnmap => {}
            BlockReg::Unused => {
                for b in ro.buf.iter_mut() {
                    *b = 0;
                }
            }
        }
        // XXX: all zeroes for now
        for b in ro.buf.iter_mut() {
            *b = 0;
        }
    }
}
impl VirtioDevice for VirtioBlock {
    fn device_cfg_size() -> usize {
        0x3c
    }
    fn device_cfg_read(&self, ro: &mut ReadOp) {
        BLOCK_DEV_REGS.read(ro, |id, ro| self.block_cfg_read(id, ro));
    }

    fn device_cfg_write(&self, _wo: &WriteOp) {
        // ignore writes
    }

    fn device_get_features(&self) -> u32 {
        // XXX: real features
        0
    }
    fn device_set_features(&self, feat: u32) {
        // XXX: real features
    }

    fn device_id_and_class() -> (u16, u8) {
        // block device, storage class
        (VIRTIO_DEV_BLOCK, 0x01)
    }
}

#[derive(Copy, Clone, Eq, PartialEq, Debug)]
enum BlockReg {
    Capacity,
    SizeMax,
    SegMax,
    GeoCyl,
    GeoHeads,
    GeoSectors,
    BlockSize,
    TopoPhysExp,
    TopoAlignOff,
    TopoMinIoSz,
    TopoOptIoSz,
    Writeback,
    Unused,
    MaxDiscardSectors,
    MaxDiscardSeg,
    DiscardSectorAlign,
    MaxZeroSectors,
    MaxZeroSeg,
    ZeroMayUnmap,
}
lazy_static! {
    static ref BLOCK_DEV_REGS: RegMap<BlockReg> = {
        let layout = [
            (BlockReg::Capacity, 8),
            (BlockReg::SizeMax, 4),
            (BlockReg::SegMax, 4),
            (BlockReg::GeoCyl, 2),
            (BlockReg::GeoHeads, 1),
            (BlockReg::GeoSectors, 1),
            (BlockReg::BlockSize, 4),
            (BlockReg::TopoPhysExp, 1),
            (BlockReg::TopoAlignOff, 1),
            (BlockReg::TopoMinIoSz, 2),
            (BlockReg::TopoOptIoSz, 4),
            (BlockReg::Writeback, 1),
            (BlockReg::Unused, 3),
            (BlockReg::MaxDiscardSectors, 4),
            (BlockReg::MaxDiscardSeg, 4),
            (BlockReg::DiscardSectorAlign, 4),
            (BlockReg::MaxZeroSectors, 4),
            (BlockReg::MaxZeroSeg, 4),
            (BlockReg::ZeroMayUnmap, 1),
            (BlockReg::Unused, 3),
        ];
        let size = 0x3c;
        RegMap::create_packed(size, &layout, Some(BlockReg::Unused))
    };
}
