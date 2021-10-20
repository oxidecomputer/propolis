use std::any::Any;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, Weak};

use super::bits::*;
use super::queue::VirtQueue;
use super::{VirtioDevice, VirtioIntr, VqChange, VqIntr};
use crate::common::*;
use crate::dispatch::DispCtx;
use crate::hw::pci;
use crate::instance;
use crate::intr_pins::IntrPin;
use crate::util::regmap::RegMap;

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
    intr_mode: IntrMode,
    intr_mode_updating: bool,
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
            intr_mode: IntrMode::IsrOnly,
            intr_mode_updating: false,
            msix_cfg_vec: VIRTIO_MSI_NO_VECTOR,
            msix_queue_vec,
        }
    }
    fn reset(&mut self) {
        self.status = Status::RESET;
        self.queue_sel = 0;
        self.nego_feat = 0;
        self.msix_cfg_vec = VIRTIO_MSI_NO_VECTOR;
    }
}

pub struct PciVirtio {
    pci_state: pci::DeviceState,

    map: RegMap<VirtioTop>,
    map_nomsix: RegMap<VirtioTop>,
    /// Quick access to register map for MSIX (true) or non-MSIX (false)
    map_which: AtomicBool,

    state: Mutex<VirtioState>,
    state_cv: Condvar,
    isr_state: Arc<IsrState>,

    dev: Arc<dyn VirtioDevice>,
    dev_any: Arc<dyn Any + Send + Sync + 'static>,
}
impl PciVirtio {
    pub(super) fn create<D>(
        msix_count: Option<u16>,
        dev_id: u16,
        dev_class: u8,
        cfg_sz: usize,
        inner: Arc<D>,
    ) -> Arc<Self>
    where
        D: VirtioDevice + Send + Sync + 'static,
    {
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
        builder = builder.add_bar_io(pci::BarN::BAR0, 0x200);
        let pci_state = builder.finish();

        let layout = [
            (VirtioTop::LegacyConfig, LEGACY_REG_SZ),
            (VirtioTop::DeviceConfig, cfg_sz),
        ];
        let layout_nomsix = [
            (VirtioTop::LegacyConfig, LEGACY_REG_SZ_NO_MSIX),
            (VirtioTop::DeviceConfig, cfg_sz),
        ];

        let dev_any =
            Arc::clone(&inner) as Arc<dyn Any + Send + Sync + 'static>;
        let this = Arc::new(Self {
            pci_state,

            map: RegMap::create_packed_passthru(
                cfg_sz + LEGACY_REG_SZ,
                &layout,
            ),
            map_nomsix: RegMap::create_packed_passthru(
                cfg_sz + LEGACY_REG_SZ_NO_MSIX,
                &layout_nomsix,
            ),
            map_which: AtomicBool::new(false),

            state: Mutex::new(VirtioState::new(inner.queues().count().get())),
            state_cv: Condvar::new(),
            isr_state: IsrState::new(),

            dev: inner,
            dev_any,
        });

        for queue in this.dev.queues()[..].iter() {
            queue.set_interrupt(IsrIntr::new(&this.isr_state));
        }

        this
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
                if let Some(queue) = self.vq(state.queue_sel) {
                    let addr = queue.ctrl.lock().unwrap().gpa_desc.0;
                    ro.write_u32((addr >> PAGE_SHIFT) as u32);
                } else {
                    // bogus queue
                    ro.write_u32(0);
                }
            }
            LegacyReg::QueueSize => {
                ro.write_u16(self.dev.queues().queue_size().get());
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
                // reading ISR Status clears it as well
                let isr = self.isr_state.read_clear();
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
                if let Some(queue) = self.vq(state.queue_sel) {
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
                let hdl = self.pci_state.msix_hdl().unwrap();
                let mut state = self.state.lock().unwrap();
                let sel = state.queue_sel as usize;
                if let Some(queue) = self.vq(state.queue_sel) {
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
            self.virtio_reset(state, ctx)
        } else {
            // XXX: better device status FSM
            state.status = val;
        }
    }
    fn queue_notify(&self, queue: u16, ctx: &DispCtx) {
        probe_virtio_vq_notify!(|| (self as *const PciVirtio as u64, queue));
        if let Some(vq) = self.vq(queue) {
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

    /// Reset the virtio portion of the device
    ///
    /// This leaves PCI state (such as configured BARs) unchanged
    fn virtio_reset(&self, mut state: MutexGuard<VirtioState>, ctx: &DispCtx) {
        for queue in self.dev.queues()[..].iter() {
            queue.reset();
            self.queue_change(queue, VqChange::Reset, ctx);
        }
        state.reset();
        let _ = self.isr_state.read_clear();
        self.dev.reset(ctx);
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
                self.isr_state.disable();
            }
            IntrMode::Msi => {
                // When leaving MSI mode, re-wire the Isr interrupt handling
                //
                // To avoid deadlock, the state lock must be dropped while
                // updating the interrupts handlers on queues.
                drop(state);
                for queue in self.dev.queues()[..].iter() {
                    queue.set_interrupt(IsrIntr::new(&self.isr_state));
                }
                state = self.state.lock().unwrap();
            }
            _ => {}
        }

        state.intr_mode = new_mode;
        match new_mode {
            IntrMode::IsrLintr => {
                self.isr_state.enable();
            }
            IntrMode::Msi => {
                let hdl = self.pci_state.msix_hdl().unwrap();
                for (idx, queue) in self.dev.queues()[..].iter().enumerate() {
                    let vec = *state.msix_queue_vec.get(idx).unwrap();

                    // State lock cannot be held while updating queue interrupt
                    // handlers due to deadlock possibility.
                    drop(state);
                    queue.set_interrupt(MsiIntr::new(hdl.clone(), vec));
                    state = self.state.lock().unwrap();
                }
            }
            _ => {}
        }
        state.intr_mode_updating = false;
        self.state_cv.notify_all();
    }

    fn vq(&self, qid: u16) -> Option<&Arc<VirtQueue>> {
        self.dev.queues().get(qid)
    }

    /// Get access to the inner device emulation.
    ///
    /// This will panic if the provided type does not match.
    pub fn inner_dev<T: Send + Sync + 'static>(&self) -> Arc<T> {
        let inner = Arc::clone(&self.dev_any);
        Arc::downcast(inner).unwrap()
    }
}

impl pci::Device for PciVirtio {
    fn device_state(&self) -> &pci::DeviceState {
        &self.pci_state
    }
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
    fn attach(&self) {
        if let Some(pin) = self.pci_state.lintr_pin() {
            self.isr_state.set_pin(pin);
        }
    }
    fn interrupt_mode_change(&self, mode: pci::IntrMode) {
        self.set_intr_mode(match mode {
            pci::IntrMode::Disabled => IntrMode::IsrOnly,
            pci::IntrMode::INTxPin => IntrMode::IsrLintr,
            pci::IntrMode::Msix => IntrMode::Msi,
        });

        // Make sure the correct legacy register map is used
        self.map_which.store(mode == pci::IntrMode::Msix, Ordering::SeqCst);
    }
    fn msi_update(&self, info: pci::MsiUpdate, ctx: &DispCtx) {
        let mut state = self.state.lock().unwrap();
        if state.intr_mode != IntrMode::Msi {
            return;
        }
        state =
            self.state_cv.wait_while(state, |s| s.intr_mode_updating).unwrap();
        state.intr_mode_updating = true;

        for vq in self.dev.queues()[..].iter() {
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
impl Entity for PciVirtio {
    fn state_transition(
        &self,
        next: instance::State,
        target: Option<instance::State>,
        ctx: &DispCtx,
    ) {
        self.dev.state_transition(next, target, ctx)
    }
    fn reset(&self, ctx: &DispCtx) {
        let state = self.state.lock().unwrap();
        self.virtio_reset(state, ctx);
        self.pci_state.reset(self);
    }
}

#[derive(Default)]
struct IsrInner {
    disabled: bool,
    value: u8,
    pin: Option<Arc<dyn IntrPin>>,
}
struct IsrState {
    inner: Mutex<IsrInner>,
}
impl IsrState {
    fn new() -> Arc<Self> {
        Arc::new(Self { inner: Mutex::new(IsrInner::default()) })
    }
    fn raise(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.value |= 1;
        if !inner.disabled {
            if let Some(pin) = inner.pin.as_ref() {
                pin.assert()
            }
        }
    }
    fn read_clear(&self) -> u8 {
        let mut inner = self.inner.lock().unwrap();
        let val = inner.value;
        if val != 0 {
            inner.value = 0;
            if let Some(pin) = inner.pin.as_ref() {
                pin.deassert();
            }
        }
        val
    }
    fn disable(&self) {
        let mut inner = self.inner.lock().unwrap();
        if !inner.disabled {
            if let Some(pin) = inner.pin.as_ref() {
                pin.deassert();
            }
            inner.disabled = true;
        }
    }
    fn enable(&self) {
        let mut inner = self.inner.lock().unwrap();
        if inner.disabled {
            if inner.value != 0 {
                if let Some(pin) = inner.pin.as_ref() {
                    pin.deassert();
                }
            }
            inner.disabled = false;
        }
    }
    fn set_pin(&self, pin: Arc<dyn IntrPin>) {
        let mut inner = self.inner.lock().unwrap();
        let old = inner.pin.replace(pin);
        // XXX: strict for now
        assert!(old.is_none());
    }
}

struct IsrIntr {
    state: Weak<IsrState>,
}
impl IsrIntr {
    fn new(state: &Arc<IsrState>) -> Box<Self> {
        Box::new(Self { state: Arc::downgrade(state) })
    }
}
impl VirtioIntr for IsrIntr {
    fn notify(&self, _ctx: &DispCtx) {
        if let Some(state) = Weak::upgrade(&self.state) {
            state.raise()
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
            VqIntr::Msi(data.addr, data.data, data.masked)
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
