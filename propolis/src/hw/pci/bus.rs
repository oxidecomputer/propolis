use std::collections::BTreeMap;
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
        inner.attach(bdf, dev.clone());

        let attached =
            Attachment { inner: Arc::downgrade(&self.inner), bdf, lintr_cfg };
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
}

const SLOTS_PER_BUS: usize = 32;
const FUNCS_PER_SLOT: usize = 8;

#[derive(Default)]
struct Slot {
    funcs: [Option<Arc<dyn Endpoint>>; FUNCS_PER_SLOT],
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
        Self {
            slots: Default::default(),
            bar_state: BTreeMap::new(),
            bus_pio: Arc::downgrade(pio),
            bus_mmio: Arc::downgrade(mmio),
        }
    }
    fn device_at(&self, bdf: Bdf) -> Option<Arc<dyn Endpoint>> {
        let res = self.slots[bdf.dev.get() as usize].funcs
            [bdf.func.get() as usize]
            .as_ref()
            .map(|d| Arc::clone(d));
        res
    }
    fn attach(&mut self, bdf: Bdf, dev: Arc<dyn Endpoint>) {
        let _old = self.slots[bdf.dev.get() as usize].funcs
            [bdf.func.get() as usize]
            .replace(dev);
        // XXX be strict for now
        assert!(matches!(_old, None));
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
}
