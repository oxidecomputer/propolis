use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, Weak};

use super::bits::*;
use super::queue::VirtQueue;
use super::{VirtioDevice, VirtioIntr, VqChange, VqIntr};
use crate::common::*;
use crate::dispatch::DispCtx;
use crate::hw::pci;
use crate::util::regmap::RegMap;
use crate::util::self_arc::*;

use lazy_static::lazy_static;

const VIRTIO_VENDOR: u16 = 0x1af4;

const VIRTIO_MSI_NO_VECTOR: u16 = 0xffff;

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

#[derive(Copy, Clone, Eq, PartialEq)]
enum IntrMode {
    IsrOnly,
    IsrLintr,
    Msi,
}

struct VirtioState {
    status: Status,
    queue_sel: u16,
    nego_feat: u32,
    isr_status: u8,
    intr_mode: IntrMode,
    intr_mode_updating: bool,
    lintr_pin: Option<pci::INTxPin>,
    msix_hdl: Option<pci::MsixHdl>,
    msix_cfg_vec: u16,
    msix_queue_vec: Vec<u16>,
}
impl VirtioState {
    fn new(num_queues: u16) -> Self {
        let mut msix_queue_vec = Vec::with_capacity(num_queues as usize);
        msix_queue_vec
            .resize_with(num_queues as usize, || VIRTIO_MSI_NO_VECTOR);
        Self {
            status: Status::RESET,
            queue_sel: 0,
            nego_feat: 0,
            isr_status: 0,
            intr_mode: IntrMode::IsrOnly,
            intr_mode_updating: false,
            lintr_pin: None,
            msix_hdl: None,
            msix_cfg_vec: VIRTIO_MSI_NO_VECTOR,
            msix_queue_vec,
        }
    }
    fn reset(&mut self) {
        self.status = Status::RESET;
        self.queue_sel = 0;
        self.nego_feat = 0;
        self.isr_status = 0;
        if let Some(pin) = self.lintr_pin.as_ref() {
            pin.deassert();
        }
        self.msix_cfg_vec = VIRTIO_MSI_NO_VECTOR;
    }
}

pub struct PciVirtio {
    map: RegMap<VirtioTop>,
    map_nomsix: RegMap<VirtioTop>,
    /// Quick access to register map for MSIX (true) or non-MSIX (false)
    map_which: AtomicBool,

    state: Mutex<VirtioState>,
    state_cv: Condvar,
    queue_size: u16,
    queues: Vec<Arc<VirtQueue>>,

    sa_cell: SelfArcCell<Self>,

    dev: Arc<dyn VirtioDevice>,
}
impl PciVirtio {
    pub fn create(
        queue_size: u16,
        num_queues: u16,
        msix_count: Option<u16>,
        dev_id: u16,
        dev_class: u8,
        cfg_sz: usize,
        inner: Arc<dyn VirtioDevice>,
    ) -> Arc<pci::DeviceInst> {
        assert!(queue_size > 1 && queue_size.is_power_of_two());

        let mut queues = Vec::new();
        for id in 0..num_queues {
            queues.push(Arc::new(VirtQueue::new(id, queue_size)));
        }

        let layout = [
            (VirtioTop::LegacyConfig, LEGACY_REG_SZ),
            (VirtioTop::DeviceConfig, cfg_sz),
        ];
        let layout_nomsix = [
            (VirtioTop::LegacyConfig, LEGACY_REG_SZ_NO_MSIX),
            (VirtioTop::DeviceConfig, cfg_sz),
        ];

        let mut this = Arc::new(Self {
            map: RegMap::create_packed_passthru(
                cfg_sz + LEGACY_REG_SZ,
                &layout,
            ),
            map_nomsix: RegMap::create_packed_passthru(
                cfg_sz + LEGACY_REG_SZ_NO_MSIX,
                &layout_nomsix,
            ),
            map_which: AtomicBool::new(false),

            state: Mutex::new(VirtioState::new(num_queues)),
            state_cv: Condvar::new(),
            queue_size,
            queues,

            dev: inner,

            sa_cell: SelfArcCell::new(),
        });
        SelfArc::self_arc_init(&mut this);

        for queue in this.queues.iter() {
            queue.set_interrupt(IsrIntr::new(this.self_weak()));
        }

        let mut builder = pci::Builder::new(pci::Ident {
            vendor_id: VIRTIO_VENDOR,
            device_id: dev_id,
            sub_vendor_id: VIRTIO_VENDOR,
            sub_device_id: dev_id - 0xfff,
            class: dev_class,
            ..Default::default()
        })
        .add_lintr();

        if let Some(count) = msix_count {
            builder = builder.add_cap_msix(pci::BarN::BAR1, count);
        }

        // XXX: properly size the legacy cfg BAR
        builder.add_bar_io(pci::BarN::BAR0, 0x200).finish(this)
    }

    fn legacy_read(&self, id: &LegacyReg, ro: &mut ReadOp, _ctx: &DispCtx) {
        match id {
            LegacyReg::FeatDevice => {
                ro.write_u32(self.features_supported());
            }
            LegacyReg::FeatDriver => {
                let state = self.state.lock().unwrap();
                ro.write_u32(state.nego_feat);
            }
            LegacyReg::QueuePfn => {
                let state = self.state.lock().unwrap();
                if let Some(queue) = self.queues.get(state.queue_sel as usize) {
                    let addr = queue.ctrl.lock().unwrap().gpa_desc.0;
                    ro.write_u32((addr >> PAGE_SHIFT) as u32);
                } else {
                    // bogus queue
                    ro.write_u32(0);
                }
            }
            LegacyReg::QueueSize => {
                ro.write_u16(self.queue_size);
            }
            LegacyReg::QueueSelect => {
                let state = self.state.lock().unwrap();
                ro.write_u16(state.queue_sel);
            }
            LegacyReg::QueueNotify => {}
            LegacyReg::DeviceStatus => {
                let state = self.state.lock().unwrap();
                ro.write_u8(state.status.bits());
            }
            LegacyReg::IsrStatus => {
                let mut state = self.state.lock().unwrap();
                let isr = state.isr_status;
                if isr != 0 {
                    // reading ISR Status clears it as well
                    state.isr_status = 0;
                    if let Some(pin) = state.lintr_pin.as_ref() {
                        pin.deassert();
                    }
                }
                ro.write_u8(isr);
            }
            LegacyReg::MsixVectorConfig => {
                let state = self.state.lock().unwrap();
                ro.write_u16(state.msix_cfg_vec);
            }
            LegacyReg::MsixVectorQueue => {
                let state = self.state.lock().unwrap();
                let val = state
                    .msix_queue_vec
                    .get(state.queue_sel as usize)
                    .unwrap_or(&VIRTIO_MSI_NO_VECTOR);
                ro.write_u16(*val);
            }
        }
    }
    fn legacy_write(&self, id: &LegacyReg, wo: &mut WriteOp, ctx: &DispCtx) {
        match id {
            LegacyReg::FeatDriver => {
                let nego = wo.read_u32() & self.features_supported();
                let mut state = self.state.lock().unwrap();
                state.nego_feat = nego;
                self.dev.device_set_features(nego);
            }
            LegacyReg::QueuePfn => {
                let mut state = self.state.lock().unwrap();
                let mut success = false;
                let pfn = wo.read_u32();
                if let Some(queue) = self.queues.get(state.queue_sel as usize) {
                    success = queue.map_legacy((pfn as u64) << PAGE_SHIFT);
                    self.queue_change(queue, VqChange::Address, ctx);
                }
                if !success {
                    // XXX: interrupt needed?
                    state.status |= Status::FAILED;
                }
            }
            LegacyReg::QueueSelect => {
                let mut state = self.state.lock().unwrap();
                state.queue_sel = wo.read_u16();
            }
            LegacyReg::QueueNotify => {
                self.queue_notify(wo.read_u16(), ctx);
            }
            LegacyReg::DeviceStatus => {
                self.set_status(wo.read_u8(), ctx);
            }
            LegacyReg::MsixVectorConfig => {
                let mut state = self.state.lock().unwrap();
                state.msix_cfg_vec = wo.read_u16();
            }
            LegacyReg::MsixVectorQueue => {
                let mut state = self.state.lock().unwrap();
                let sel = state.queue_sel as usize;
                if let Some(queue) = self.queues.get(sel) {
                    let val = wo.read_u16();

                    if state.intr_mode != IntrMode::Msi {
                        // Store the vector information for later
                        state.msix_queue_vec[sel] = val;
                    } else {
                        state = self
                            .state_cv
                            .wait_while(state, |s| s.intr_mode_updating)
                            .unwrap();
                        state.intr_mode_updating = true;
                        state.msix_queue_vec[sel] = val;
                        let hdl = state.msix_hdl.as_ref().unwrap().clone();

                        // State lock cannot be held while updating queue
                        // interrupt handlers due to deadlock possibility.
                        drop(state);
                        queue.set_interrupt(MsiIntr::new(hdl, val));
                        state = self.state.lock().unwrap();

                        state.intr_mode_updating = false;
                        self.state_cv.notify_all();
                    }
                }
            }

            LegacyReg::FeatDevice
            | LegacyReg::QueueSize
            | LegacyReg::IsrStatus => {
                // Read-only regs
            }
        }
    }

    fn features_supported(&self) -> u32 {
        self.dev.device_get_features() | VIRTIO_F_RING_INDIRECT_DESC as u32
    }
    fn set_status(&self, status: u8, ctx: &DispCtx) {
        let mut state = self.state.lock().unwrap();
        let val = Status::from_bits_truncate(status);
        if val == Status::RESET && state.status != Status::RESET {
            self.device_reset(state, ctx)
        } else {
            // XXX: better device status FSM
            state.status = val;
        }
    }
    fn queue_notify(&self, queue: u16, ctx: &DispCtx) {
        if let Some(vq) = self.queues.get(queue as usize) {
            self.dev.queue_notify(vq, ctx);
        }
    }
    fn queue_change(
        &self,
        vq: &Arc<VirtQueue>,
        change: VqChange,
        ctx: &DispCtx,
    ) {
        self.dev.queue_change(vq, change, ctx);
    }
    fn device_reset(&self, mut state: MutexGuard<VirtioState>, ctx: &DispCtx) {
        for queue in self.queues.iter() {
            queue.reset();
            self.queue_change(queue, VqChange::Reset, ctx);
        }
        state.reset();
        self.dev.device_reset(ctx);
    }

    fn raise_isr(&self) {
        let mut state = self.state.lock().unwrap();
        state.isr_status |= 1;
        if let Some(pin) = state.lintr_pin.as_ref() {
            pin.assert()
        }
    }

    fn set_intr_mode(&self, new_mode: IntrMode) {
        let mut state = self.state.lock().unwrap();
        let old_mode = state.intr_mode;
        if new_mode == old_mode {
            return;
        }

        state =
            self.state_cv.wait_while(state, |s| s.intr_mode_updating).unwrap();

        state.intr_mode_updating = true;
        match old_mode {
            IntrMode::IsrLintr => {
                // When leaving lintr-pin mode, deassert anything on said pin
                if let Some(pin) = state.lintr_pin.as_ref() {
                    pin.deassert();
                }
            }
            IntrMode::Msi => {
                // When leaving MSI mode, re-wire the Isr interrupt handling
                //
                // To avoid deadlock, the state lock must be dropped while
                // updating the interrupts handlers on queues.
                drop(state);
                for queue in self.queues.iter() {
                    queue.set_interrupt(IsrIntr::new(self.self_weak()));
                }
                state = self.state.lock().unwrap();
            }
            _ => {}
        }

        state.intr_mode = new_mode;
        match new_mode {
            IntrMode::IsrLintr => {
                if let Some(pin) = state.lintr_pin.as_ref() {
                    if state.isr_status != 0 {
                        pin.assert()
                    }
                }
            }
            IntrMode::Msi => {
                for (idx, queue) in self.queues.iter().enumerate() {
                    let vec = *state.msix_queue_vec.get(idx).unwrap();
                    let hdl = state.msix_hdl.as_ref().unwrap().clone();

                    // State lock cannot be held while updating queue interrupt
                    // handlers due to deadlock possibility.
                    drop(state);
                    queue.set_interrupt(MsiIntr::new(hdl, vec));
                    state = self.state.lock().unwrap();
                }
            }
            _ => {}
        }
        state.intr_mode_updating = false;
        self.state_cv.notify_all();
    }
}

impl SelfArc for PciVirtio {
    fn self_arc_cell(&self) -> &SelfArcCell<Self> {
        &self.sa_cell
    }
}

impl pci::Device for PciVirtio {
    fn bar_rw(&self, bar: pci::BarN, mut rwo: RWOp, ctx: &DispCtx) {
        assert_eq!(bar, pci::BarN::BAR0);
        let map = match self.map_which.load(Ordering::SeqCst) {
            false => &self.map_nomsix,
            true => &self.map,
        };
        map.process(&mut rwo, |id, mut rwo| match id {
            VirtioTop::LegacyConfig => {
                LEGACY_REGS.process(&mut rwo, |id, rwo| match rwo {
                    RWOp::Read(ro) => self.legacy_read(id, ro, ctx),
                    RWOp::Write(wo) => self.legacy_write(id, wo, ctx),
                })
            }
            VirtioTop::DeviceConfig => self.dev.device_cfg_rw(rwo),
        });
    }
    fn attach(
        &self,
        lintr_pin: Option<pci::INTxPin>,
        msix_hdl: Option<pci::MsixHdl>,
    ) {
        let mut state = self.state.lock().unwrap();
        state.lintr_pin = lintr_pin;
        state.msix_hdl = msix_hdl;
        self.dev.attach(&self.queues[..]);
    }
    fn interrupt_mode_change(&self, mode: pci::IntrMode) {
        self.set_intr_mode(match mode {
            pci::IntrMode::Disabled => IntrMode::IsrOnly,
            pci::IntrMode::INTxPin => IntrMode::IsrLintr,
            pci::IntrMode::MSIX => IntrMode::Msi,
        });

        // Make sure the correct legacy register map is used
        self.map_which.store(mode == pci::IntrMode::MSIX, Ordering::SeqCst);
    }
    fn msi_update(&self, info: pci::MsiUpdate, ctx: &DispCtx) {
        let mut state = self.state.lock().unwrap();
        if state.intr_mode != IntrMode::Msi {
            return;
        }
        state =
            self.state_cv.wait_while(state, |s| s.intr_mode_updating).unwrap();
        state.intr_mode_updating = true;

        for vq in self.queues.iter() {
            let val = *state.msix_queue_vec.get(vq.id as usize).unwrap();

            // avoid deadlock while modify per-VQ interrupt config
            drop(state);

            match info {
                pci::MsiUpdate::MaskAll | pci::MsiUpdate::UnmaskAll
                    if val != VIRTIO_MSI_NO_VECTOR =>
                {
                    self.queue_change(vq, VqChange::IntrCfg, ctx);
                }
                pci::MsiUpdate::Modify(idx) if val == idx => {
                    self.queue_change(vq, VqChange::IntrCfg, ctx);
                }
                _ => {}
            }

            state = self.state.lock().unwrap();
        }
        state.intr_mode_updating = false;
        self.state_cv.notify_all();
    }
}

struct IsrIntr {
    outer: Weak<PciVirtio>,
}
impl IsrIntr {
    fn new(outer: Weak<PciVirtio>) -> Box<Self> {
        Box::new(Self { outer })
    }
}
impl VirtioIntr for IsrIntr {
    fn notify(&self, _ctx: &DispCtx) {
        if let Some(dev) = Weak::upgrade(&self.outer) {
            dev.raise_isr();
        }
    }
    fn read(&self) -> VqIntr {
        VqIntr::Pin
    }
}

struct MsiIntr {
    hdl: pci::MsixHdl,
    index: u16,
}
impl MsiIntr {
    fn new(hdl: pci::MsixHdl, index: u16) -> Box<Self> {
        Box::new(Self { hdl, index })
    }
}
impl VirtioIntr for MsiIntr {
    fn notify(&self, ctx: &DispCtx) {
        if self.index < self.hdl.count() {
            self.hdl.fire(self.index, ctx);
        }
    }
    fn read(&self) -> VqIntr {
        if self.index < self.hdl.count() {
            let data = self.hdl.read(self.index);
            VqIntr::MSI(data.addr, data.data, data.masked)
        } else {
            VqIntr::Pin
        }
    }
}

#[derive(Copy, Clone, Eq, PartialEq, Debug)]
enum VirtioTop {
    LegacyConfig,
    DeviceConfig,
}

const LEGACY_REG_SZ: usize = 0x18;
const LEGACY_REG_SZ_NO_MSIX: usize = 0x14;

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
        RegMap::create_packed(LEGACY_REG_SZ, &layout, None)
    };
}
