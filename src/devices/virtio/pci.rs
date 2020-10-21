use std::sync::{Arc, Mutex, MutexGuard, Weak};

use super::queue::VirtQueue;
use super::{VirtioDevice, VirtioIntr};
use crate::common::*;
use crate::dispatch::DispCtx;
use crate::pci;
use crate::util::regmap::RegMap;
use crate::util::self_arc::*;

use byteorder::{ByteOrder, LE};
use lazy_static::lazy_static;

const VIRTIO_VENDOR: u16 = 0x1af4;

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
    lintr_pin: Option<pci::INTxPin>,
}
impl VirtioState {
    fn new() -> Self {
        Self {
            status: Status::RESET,
            queue_sel: 0,
            nego_feat: 0,
            isr_status: 0,
            lintr_pin: None,
        }
    }
}

pub struct PciVirtio<D: VirtioDevice + 'static> {
    map: RegMap<VirtioTop>,
    state: Mutex<VirtioState>,
    queue_size: u16,
    num_queues: u16,
    queues: Vec<Arc<VirtQueue>>,

    sa_cell: SelfArcCell<Self>,

    dev: D,
}
impl<D: VirtioDevice + 'static> PciVirtio<D> {
    fn map_for_device() -> RegMap<VirtioTop> {
        let dev_sz = D::device_cfg_size();
        // XXX: Shortened for no-msix
        let legacy_sz = 0x14;
        let layout = [
            (VirtioTop::LegacyConfig, legacy_sz),
            (VirtioTop::DeviceConfig, dev_sz),
        ];
        let size = dev_sz + legacy_sz;
        RegMap::create_packed_passthru(size, &layout)
    }

    pub fn new(
        queue_size: u16,
        num_queues: u16,
        inner: D,
    ) -> Arc<pci::DeviceInst> {
        assert!(queue_size > 1 && queue_size.is_power_of_two());

        let mut queues = Vec::new();
        for _ in 0..num_queues {
            queues.push(Arc::new(VirtQueue::new(queue_size)));
        }

        let mut this = Arc::new(Self {
            map: Self::map_for_device(),
            state: Mutex::new(VirtioState::new()),
            queue_size,
            num_queues,
            queues,

            dev: inner,

            sa_cell: SelfArcCell::new(),
        });
        SelfArc::self_arc_init(&mut this);

        for queue in this.queues.iter() {
            queue.set_interrupt(Box::new(QueueIntr {
                action: IntrAction::Isr,
                outer: this.self_weak(),
            }));
        }

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
        .finish_arc(this)
    }

    fn legacy_read(&self, id: &LegacyReg, ro: &mut ReadOp, _ctx: &DispCtx) {
        match id {
            LegacyReg::FeatDevice => {
                LE::write_u32(ro.buf, self.features_supported());
            }
            LegacyReg::FeatDriver => {
                let state = self.state.lock().unwrap();
                LE::write_u32(ro.buf, state.nego_feat);
            }
            LegacyReg::QueuePfn => {
                let state = self.state.lock().unwrap();
                if let Some(queue) = self.queues.get(state.queue_sel as usize) {
                    let addr = queue.ctrl.lock().unwrap().gpa_desc.0;
                    LE::write_u32(ro.buf, (addr >> PAGE_SHIFT) as u32);
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
                let isr = state.isr_status;
                if isr != 0 {
                    // reading ISR Status clears it as well
                    state.isr_status = 0;
                    state.lintr_pin.as_ref().map(|i| i.deassert());
                }
                ro.buf[0] = isr;
            }
            _ => {
                // no msix for now
            }
        }
    }
    fn legacy_write(&self, id: &LegacyReg, wo: &WriteOp, ctx: &DispCtx) {
        match id {
            LegacyReg::FeatDriver => {
                let nego = LE::read_u32(wo.buf) & self.features_supported();
                let mut state = self.state.lock().unwrap();
                state.nego_feat = nego;
                self.dev.device_set_features(nego);
            }
            LegacyReg::QueuePfn => {
                let mut state = self.state.lock().unwrap();
                let mut success = false;
                let pfn = LE::read_u32(wo.buf);
                if let Some(queue) = self.queues.get(state.queue_sel as usize) {
                    success = queue.map_legacy((pfn as u64) << PAGE_SHIFT);
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
                self.queue_notify(LE::read_u16(wo.buf), ctx);
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

    fn features_supported(&self) -> u32 {
        self.dev.device_get_features() | VIRTIO_F_RING_INDIRECT_DESC
    }
    fn set_status(&self, status: u8) {
        let mut state = self.state.lock().unwrap();
        let val = Status::from_bits_truncate(status);
        if val == Status::RESET && state.status != Status::RESET {
            self.device_reset(state)
        } else {
            // XXX: better device status FSM
            state.status = val;
        }
    }
    fn queue_notify(&self, queue: u16, ctx: &DispCtx) {
        println!("notify virtqueue {}", queue);
        if let Some(vq) = self.queues.get(queue as usize) {
            self.dev.queue_notify(queue, vq, ctx);
        }
    }
    fn device_reset(&self, mut state: MutexGuard<VirtioState>) {
        for queue in self.queues.iter() {
            queue.reset();
        }
        state.nego_feat = 0;
        state.queue_sel = 0;
        state.isr_status = 0;
        state.status = Status::RESET;
    }

    fn raise_isr(&self) {
        let mut state = self.state.lock().unwrap();
        state.isr_status |= 1;
        if let Some(pin) = state.lintr_pin.as_ref() {
            pin.assert()
        }
    }
}

impl<D: VirtioDevice> SelfArc for PciVirtio<D> {
    fn self_arc_cell(&self) -> &SelfArcCell<Self> {
        &self.sa_cell
    }
}

impl<D: VirtioDevice> pci::Device for PciVirtio<D> {
    fn bar_rw(&self, bar: pci::BarN, rwo: &mut RWOp, ctx: &DispCtx) {
        assert_eq!(bar, pci::BarN::BAR0);
        self.map.process(rwo, |id, rwo| match id {
            VirtioTop::LegacyConfig => {
                LEGACY_REGS.process(rwo, |id, rwo| match rwo {
                    RWOp::Read(ro) => self.legacy_read(id, ro, ctx),
                    RWOp::Write(wo) => self.legacy_write(id, wo, ctx),
                })
            }
            VirtioTop::DeviceConfig => self.dev.device_cfg_rw(rwo),
        });
    }
    fn intr_mode_change(&self, mode: pci::IntrMode) {
        match mode {
            pci::IntrMode::Disabled => {
                let mut state = self.state.lock().unwrap();
                if let Some(pin) = std::mem::replace(&mut state.lintr_pin, None)
                {
                    pin.deassert();
                }
            }
            pci::IntrMode::INTxPin(pin) => {
                let mut state = self.state.lock().unwrap();
                assert!(state.lintr_pin.is_none());
                if state.isr_status != 0 {
                    pin.assert();
                }
                state.lintr_pin = Some(pin);
            }
            pci::IntrMode::MSIX => {
                todo!("add msix support");
            }
        }
    }
}

enum IntrAction {
    Isr,
    Msi,
}

struct QueueIntr<D: VirtioDevice> {
    action: IntrAction,
    outer: Weak<PciVirtio<D>>,
}

impl<D: VirtioDevice> VirtioIntr for QueueIntr<D> {
    fn notify(&self) {
        match self.action {
            IntrAction::Isr => {
                if let Some(dev) = Weak::upgrade(&self.outer) {
                    dev.raise_isr();
                }
            }
            IntrAction::Msi => todo!("wire up MSI"),
        }
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
    QueuePfn,
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
            (LegacyReg::QueuePfn, 4),
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
