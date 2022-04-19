use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};

use super::bar::BarDefine;
use super::{BarN, Bdf, BusNum, Endpoint, LintrCfg};
use crate::common::RWOp;
use crate::dispatch::DispCtx;
use crate::mmio::{MmioBus, MmioFn};
use crate::pio::{PioBus, PioFn};

pub struct Bus {
    n: BusNum,
    inner: Arc<Mutex<Inner>>,
}

impl Bus {
    pub fn new(n: BusNum, pio: &Arc<PioBus>, mmio: &Arc<MmioBus>) -> Self {
        Self { n, inner: Arc::new(Mutex::new(Inner::new(pio, mmio))) }
    }

    pub fn attach(
        &self,
        bdf: Bdf,
        dev: Arc<dyn Endpoint>,
        lintr_cfg: Option<LintrCfg>,
    ) {
        assert_eq!(bdf.bus, self.n);

        let mut inner = self.inner.lock().unwrap();
        let slot_state = inner.attach(bdf, dev.clone());

        let attached = Attachment {
            inner: Arc::downgrade(&self.inner),
            bdf,
            lintr_cfg,
            slot_state,
        };
        dev.attach(attached);
    }

    pub fn device_at(&self, bdf: Bdf) -> Option<Arc<dyn Endpoint>> {
        assert_eq!(bdf.bus, self.n);

        let inner = self.inner.lock().unwrap();
        inner.device_at(bdf)
    }
}

pub struct Attachment {
    inner: Weak<Mutex<Inner>>,
    bdf: Bdf,
    lintr_cfg: Option<LintrCfg>,
    slot_state: Arc<SlotState>,
}
impl Attachment {
    pub fn bar_register(&self, n: BarN, def: BarDefine, addr: u64) {
        if let Some(inner) = self.inner.upgrade() {
            let mut guard = inner.lock().unwrap();
            guard.bar_register(self.bdf, n, def, addr);
        }
    }
    pub fn bar_unregister(&self, n: BarN) {
        if let Some(inner) = self.inner.upgrade() {
            let mut guard = inner.lock().unwrap();
            guard.bar_unregister(self.bdf, n);
        }
    }
    pub fn lintr_cfg(&self) -> Option<&LintrCfg> {
        self.lintr_cfg.as_ref()
    }
    pub fn bdf(&self) -> Bdf {
        self.bdf
    }
    pub fn is_multifunc(&self) -> bool {
        self.slot_state.is_multifunc.load(Ordering::Acquire)
    }
}

#[derive(Default)]
struct SlotState {
    is_multifunc: AtomicBool,
}

const SLOTS_PER_BUS: usize = 32;
const FUNCS_PER_SLOT: usize = 8;

#[derive(Default)]
struct Slot {
    funcs: [Option<Arc<dyn Endpoint>>; FUNCS_PER_SLOT],
    state: Arc<SlotState>,
}
impl Slot {
    fn attach(&mut self, bdf: Bdf, dev: Arc<dyn Endpoint>) -> Arc<SlotState> {
        let _old = self.funcs[bdf.func.get() as usize].replace(dev);

        // XXX be strict for now
        assert!(matches!(_old, None));

        // Keep multi-func state updated
        if !self.state.is_multifunc.load(Ordering::Acquire) {
            if self.funcs.iter().filter(|x| x.is_some()).count() > 1 {
                self.state.is_multifunc.store(true, Ordering::Release);
            }
        }
        self.state.clone()
    }
}

struct BarState {
    def: BarDefine,
    value: u64,
    live: bool,
}

struct Inner {
    slots: [Slot; SLOTS_PER_BUS],
    bar_state: BTreeMap<(Bdf, BarN), BarState>,
    bus_pio: Weak<PioBus>,
    bus_mmio: Weak<MmioBus>,
}
impl Inner {
    fn new(pio: &Arc<PioBus>, mmio: &Arc<MmioBus>) -> Self {
        let this = Self {
            slots: Default::default(),
            bar_state: BTreeMap::new(),
            bus_pio: Arc::downgrade(pio),
            bus_mmio: Arc::downgrade(mmio),
        };

        #[cfg(feature = "testonly-pci-enhanced-configuration")]
        this.enable_mmio_configuration();

        this
    }
    fn device_at(&self, bdf: Bdf) -> Option<Arc<dyn Endpoint>> {
        let res = self.slots[bdf.dev.get() as usize].funcs
            [bdf.func.get() as usize]
            .as_ref()
            .map(Arc::clone);
        res
    }
    fn attach(&mut self, bdf: Bdf, dev: Arc<dyn Endpoint>) -> Arc<SlotState> {
        self.slots[bdf.dev.get() as usize].attach(bdf, dev)
    }
    fn bar_register(&mut self, bdf: Bdf, n: BarN, def: BarDefine, value: u64) {
        let dev = self.device_at(bdf).unwrap();

        let live = match def {
            BarDefine::Pio(sz) => {
                if let Some(pio) = self.bus_pio.upgrade() {
                    let func =
                        Arc::new(move |_port: u16, rwo: RWOp, ctx: &DispCtx| {
                            dev.bar_rw(n, rwo, ctx)
                        }) as Arc<PioFn>;
                    pio.register(value as u16, sz, func).is_ok()
                } else {
                    false
                }
            }
            BarDefine::Mmio(sz) => {
                if let Some(mmio) = self.bus_mmio.upgrade() {
                    let func = Arc::new(
                        move |_addr: usize, rwo: RWOp, ctx: &DispCtx| {
                            dev.bar_rw(n, rwo, ctx)
                        },
                    ) as Arc<MmioFn>;
                    mmio.register(value as usize, sz as usize, func).is_ok()
                } else {
                    false
                }
            }
            BarDefine::Mmio64(sz) => {
                if let Some(mmio) = self.bus_mmio.upgrade() {
                    let func = Arc::new(
                        move |_addr: usize, rwo: RWOp, ctx: &DispCtx| {
                            dev.bar_rw(n, rwo, ctx)
                        },
                    ) as Arc<MmioFn>;
                    mmio.register(value as usize, sz as usize, func).is_ok()
                } else {
                    false
                }
            }
        };
        let _old =
            self.bar_state.insert((bdf, n), BarState { def, value, live });
        // XXX be strict for now
        assert!(_old.is_none());
    }
    fn bar_unregister(&mut self, bdf: Bdf, n: BarN) {
        if let Some(state) = self.bar_state.remove(&(bdf, n)) {
            if !state.live {
                // when BAR was registered, it conflicted with something else on
                // the bus, so no further action is necessary
                return;
            }
            match state.def {
                BarDefine::Pio(_) => {
                    if let Some(pio) = self.bus_pio.upgrade() {
                        pio.unregister(state.value as u16).unwrap();
                    }
                }
                BarDefine::Mmio(_) | BarDefine::Mmio64(_) => {
                    if let Some(mmio) = self.bus_mmio.upgrade() {
                        mmio.unregister(state.value as usize).unwrap();
                    }
                }
            }
        }
    }

    #[cfg(feature = "testonly-pci-enhanced-configuration")]
    fn enable_mmio_configuration(&self) {
        use crate::hw::pci::bits::{MASK_BUS, MASK_DEV, MASK_FUNC};

        if let Some(mmio) = self.bus_mmio.upgrade() {
            const ECAM_BASE_ADDRESS: usize = 0xe000_0000;
            let ecam_func = Arc::new(
                move |addr: usize, rwo: RWOp, ctx: &DispCtx| {
                    // Each function gets 4 KiB of extended configuration space,
                    // with the bus, device, and function numbers encoded in
                    // bits [27:20], [19:15], and [14:12] respectively.
                    let ecam_offset = (addr - ECAM_BASE_ADDRESS) + rwo.offset();
                    let bus = (ecam_offset >> 20) as u8 & MASK_BUS;
                    let dev = (ecam_offset >> 15) as u8 & MASK_DEV;
                    let func = (ecam_offset >> 12) as u8 & MASK_FUNC;
                    let bdf = Bdf::new(bus, dev, func);

                    // This shouldn't happen absent a code bug in the parsing
                    // logic, but don't unceremoniously zap the guest if it
                    // does.
                    if bdf.is_none() {
                        slog::error!(ctx.log, "Failed to parse PCIe extended 
                                     configuration space access into BDF";
                                     "ecam_offset" => format!("0x{:x}", ecam_offset),
                                     "bus" => bus,
                                     "dev" => dev,
                                     "func" => func);
                        return;
                    }
                    let bdf = bdf.unwrap();
                    slog::info!(ctx.log, "i'm in ur pci mmio space";
                                "addr" => format!("0x{:x}", addr + rwo.offset()),
                                "size" => format!("0x{:x}", rwo.len()),
                                "bdf" => bdf.to_string());
                },
            ) as Arc<MmioFn>;
            mmio.register(ECAM_BASE_ADDRESS, 0x1000_0000, ecam_func).unwrap();
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;

    fn prep() -> (Arc<PioBus>, Arc<MmioBus>) {
        (Arc::new(PioBus::new()), Arc::new(MmioBus::new(u32::MAX as usize)))
    }

    #[derive(Default)]
    struct TestDev {
        inner: Mutex<Option<Attachment>>,
    }
    impl Endpoint for TestDev {
        fn attach(&self, attachment: Attachment) {
            let mut attach = self.inner.lock().unwrap();
            attach.replace(attachment);
        }
        fn cfg_rw(&self, _op: RWOp, _ctx: &DispCtx) {}
        fn bar_rw(&self, _bar: BarN, _rwo: RWOp, _ctx: &DispCtx) {}
    }
    impl TestDev {
        fn check_multifunc(&self) -> Option<bool> {
            self.inner.lock().unwrap().as_ref().map(Attachment::is_multifunc)
        }
    }

    #[test]
    fn empty() {
        let (pio, mmio) = prep();
        let bus = Bus::new(BusNum::new(0).unwrap(), &pio, &mmio);

        for slot in 0..31 {
            for func in 0..7 {
                let bdf = Bdf::new(0, slot, func).unwrap();
                assert!(
                    matches!(bus.device_at(bdf), None),
                    "no device at {:?}",
                    bdf
                );
            }
        }
    }

    #[test]
    #[should_panic]
    fn bad_bus_lookup() {
        let (pio, mmio) = prep();
        let bus = Bus::new(BusNum::new(0).unwrap(), &pio, &mmio);

        let bdf = Bdf::new(1, 0, 0).unwrap();
        let _ = bus.device_at(bdf);
    }

    #[test]
    #[should_panic]
    fn bad_bus_insert() {
        let (pio, mmio) = prep();
        let bus = Bus::new(BusNum::new(0).unwrap(), &pio, &mmio);

        let dev = Arc::new(TestDev::default());
        let bdf = Bdf::new(1, 0, 0).unwrap();
        bus.attach(bdf, dev as Arc<dyn Endpoint>, None);
    }

    #[test]
    fn set_multifunc() {
        let (pio, mmio) = prep();
        let bus = Bus::new(BusNum::new(0).unwrap(), &pio, &mmio);

        let first = Arc::new(TestDev::default());
        let other_slot = Arc::new(TestDev::default());
        let same_slot = Arc::new(TestDev::default());

        bus.attach(
            Bdf::new(0, 0, 0).unwrap(),
            Arc::clone(&first) as Arc<dyn Endpoint>,
            None,
        );
        assert_eq!(first.check_multifunc(), Some(false));

        bus.attach(
            Bdf::new(0, 1, 0).unwrap(),
            Arc::clone(&other_slot) as Arc<dyn Endpoint>,
            None,
        );
        assert_eq!(first.check_multifunc(), Some(false));
        assert_eq!(other_slot.check_multifunc(), Some(false));

        bus.attach(
            Bdf::new(0, 0, 1).unwrap(),
            Arc::clone(&same_slot) as Arc<dyn Endpoint>,
            None,
        );
        assert_eq!(first.check_multifunc(), Some(true));
        assert_eq!(same_slot.check_multifunc(), Some(true));
        assert_eq!(other_slot.check_multifunc(), Some(false));
    }
}
