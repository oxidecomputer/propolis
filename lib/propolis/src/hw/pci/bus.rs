// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};

use super::bar::BarDefine;
use super::{BarN, BusLocation, Endpoint, LintrCfg};
use crate::accessors::*;
use crate::common::RWOp;
use crate::mmio::{MmioBus, MmioFn};
use crate::pio::{PioBus, PioFn};

pub struct Bus {
    inner: Arc<Mutex<Inner>>,
}

impl Bus {
    pub fn new(
        pio: &Arc<PioBus>,
        mmio: &Arc<MmioBus>,
        acc_mem: MemAccessor,
        acc_msi: MsiAccessor,
    ) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner::new(
                pio, mmio, acc_mem, acc_msi,
            ))),
        }
    }

    pub fn attach(
        &self,
        location: BusLocation,
        dev: Arc<dyn Endpoint>,
        lintr_cfg: Option<LintrCfg>,
    ) {
        let mut inner = self.inner.lock().unwrap();
        let (slot_state, acc_msi, acc_mem) =
            inner.attach(location, dev.clone());

        let attached = Attachment {
            inner: Arc::downgrade(&self.inner),
            location,
            lintr_cfg,
            slot_state,
            acc_msi,
            acc_mem,
        };
        dev.attach(attached);
    }

    pub fn device_at(
        &self,
        location: BusLocation,
    ) -> Option<Arc<dyn Endpoint>> {
        let inner = self.inner.lock().unwrap();
        inner.device_at(location)
    }
}

pub struct Attachment {
    inner: Weak<Mutex<Inner>>,
    location: BusLocation,
    lintr_cfg: Option<LintrCfg>,
    slot_state: Arc<SlotState>,
    pub acc_msi: MsiAccessor,
    pub acc_mem: MemAccessor,
}
impl Attachment {
    pub fn bar_register(&self, n: BarN, def: BarDefine, addr: u64) {
        if let Some(inner) = self.inner.upgrade() {
            let mut guard = inner.lock().unwrap();
            guard.bar_register(self.location, n, def, addr);
        }
    }
    pub fn bar_unregister(&self, n: BarN) {
        if let Some(inner) = self.inner.upgrade() {
            let mut guard = inner.lock().unwrap();
            guard.bar_unregister(self.location, n);
        }
    }
    pub fn lintr_cfg(&self) -> Option<&LintrCfg> {
        self.lintr_cfg.as_ref()
    }
    pub fn location(&self) -> BusLocation {
        self.location
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
    fn attach(
        &mut self,
        location: BusLocation,
        dev: Arc<dyn Endpoint>,
    ) -> Arc<SlotState> {
        let _old = self.funcs[location.func.get() as usize].replace(dev);

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
    bar_state: BTreeMap<(BusLocation, BarN), BarState>,
    bus_pio: Weak<PioBus>,
    bus_mmio: Weak<MmioBus>,

    acc_msi: MsiAccessor,
    acc_mem: MemAccessor,
}
impl Inner {
    fn new(
        pio: &Arc<PioBus>,
        mmio: &Arc<MmioBus>,
        acc_mem: MemAccessor,
        acc_msi: MsiAccessor,
    ) -> Self {
        Self {
            slots: Default::default(),
            bar_state: BTreeMap::new(),
            bus_pio: Arc::downgrade(pio),
            bus_mmio: Arc::downgrade(mmio),

            acc_msi,
            acc_mem,
        }
    }
    fn device_at(&self, location: BusLocation) -> Option<Arc<dyn Endpoint>> {
        let res = self.slots[location.dev.get() as usize].funcs
            [location.func.get() as usize]
            .as_ref()
            .map(Arc::clone);
        res
    }
    fn attach(
        &mut self,
        location: BusLocation,
        dev: Arc<dyn Endpoint>,
    ) -> (Arc<SlotState>, MsiAccessor, MemAccessor) {
        let slot_state =
            self.slots[location.dev.get() as usize].attach(location, dev);

        let acc_name = format!(
            "PCI dev:{} func:{}",
            location.dev.get(),
            location.func.get()
        );
        (
            slot_state,
            self.acc_msi.child(Some(acc_name.clone())),
            self.acc_mem.child(Some(acc_name)),
        )
    }
    fn bar_register(
        &mut self,
        location: BusLocation,
        n: BarN,
        def: BarDefine,
        value: u64,
    ) {
        let dev = self.device_at(location).unwrap();

        let live = match def {
            BarDefine::Pio(sz) => {
                if let Some(pio) = self.bus_pio.upgrade() {
                    let func = Arc::new(move |_port: u16, rwo: RWOp| {
                        dev.bar_rw(n, rwo)
                    }) as Arc<PioFn>;
                    pio.register(value as u16, sz, func).is_ok()
                } else {
                    false
                }
            }
            BarDefine::Mmio(sz) => {
                if let Some(mmio) = self.bus_mmio.upgrade() {
                    let func = Arc::new(move |_addr: usize, rwo: RWOp| {
                        dev.bar_rw(n, rwo)
                    }) as Arc<MmioFn>;
                    mmio.register(value as usize, sz as usize, func).is_ok()
                } else {
                    false
                }
            }
            BarDefine::Mmio64(sz) => {
                if let Some(mmio) = self.bus_mmio.upgrade() {
                    let func = Arc::new(move |_addr: usize, rwo: RWOp| {
                        dev.bar_rw(n, rwo)
                    }) as Arc<MmioFn>;
                    mmio.register(value as usize, sz as usize, func).is_ok()
                } else {
                    false
                }
            }
        };
        let _old =
            self.bar_state.insert((location, n), BarState { def, value, live });
        // XXX be strict for now
        assert!(_old.is_none());
    }
    fn bar_unregister(&mut self, location: BusLocation, n: BarN) {
        if let Some(state) = self.bar_state.remove(&(location, n)) {
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

#[cfg(test)]
mod test {
    use super::*;
    use crate::hw::pci::test::Scaffold;

    #[derive(Default)]
    struct TestDev {
        inner: Mutex<Option<Attachment>>,
    }
    impl Endpoint for TestDev {
        fn attach(&self, attachment: Attachment) {
            let mut attach = self.inner.lock().unwrap();
            attach.replace(attachment);
        }
        fn cfg_rw(&self, _op: RWOp) {}
        fn bar_rw(&self, _bar: BarN, _rwo: RWOp) {}
    }
    impl TestDev {
        fn check_multifunc(&self) -> Option<bool> {
            self.inner.lock().unwrap().as_ref().map(Attachment::is_multifunc)
        }
    }

    #[test]
    fn empty() {
        let scaffold = Scaffold::new();
        let bus = scaffold.create_bus();

        for slot in 0..31 {
            for func in 0..7 {
                let location = BusLocation::new(slot, func).unwrap();
                assert!(
                    matches!(bus.device_at(location), None),
                    "no device at 0.{:?}",
                    location
                );
            }
        }
    }

    #[test]
    fn set_multifunc() {
        let scaffold = Scaffold::new();
        let bus = scaffold.create_bus();

        let first = Arc::new(TestDev::default());
        let other_slot = Arc::new(TestDev::default());
        let same_slot = Arc::new(TestDev::default());

        bus.attach(
            BusLocation::new(0, 0).unwrap(),
            Arc::clone(&first) as Arc<dyn Endpoint>,
            None,
        );
        assert_eq!(first.check_multifunc(), Some(false));

        bus.attach(
            BusLocation::new(1, 0).unwrap(),
            Arc::clone(&other_slot) as Arc<dyn Endpoint>,
            None,
        );
        assert_eq!(first.check_multifunc(), Some(false));
        assert_eq!(other_slot.check_multifunc(), Some(false));

        bus.attach(
            BusLocation::new(0, 1).unwrap(),
            Arc::clone(&same_slot) as Arc<dyn Endpoint>,
            None,
        );
        assert_eq!(first.check_multifunc(), Some(true));
        assert_eq!(same_slot.check_multifunc(), Some(true));
        assert_eq!(other_slot.check_multifunc(), Some(false));
    }
}
