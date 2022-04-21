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

    pub fn extended_cfg_rw(&self, addr: usize, rwo: RWOp, ctx: &DispCtx) {
        let inner = self.inner.lock().unwrap();
        inner.extended_config_rw(addr, rwo, ctx);
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

    fn extended_config_rw(&self, _addr: usize, rwo: RWOp, ctx: &DispCtx) {
        use crate::{
            common::{ReadOp, WriteOp},
            hw::pci::bits::{LEN_CFG, MASK_ECAM_ACCESS_ALIGN},
        };

        assert_ne!(rwo.len(), 0);
        let (bdf, cfg_offset) = super::decode_extended_cfg_offset(rwo.offset());

        // Compute the last byte that will be accessed to check for accesses
        // that span multiple doublewords. (This won't overflow because the
        // starting offset can't exceed 4 KiB, the size of a single function's
        // configuration space.)
        let cfg_last = cfg_offset + rwo.len() - 1;

        // Reject if the access if:
        // - the access touches bytes past the end of the 256-byte PCI config
        //   space (this causes a panic for legacy PCI devices), or
        // - the access illegally spans multiple words.
        if (cfg_last > LEN_CFG)
            || ((cfg_offset & MASK_ECAM_ACCESS_ALIGN)
                != (cfg_last & MASK_ECAM_ACCESS_ALIGN))
        {
            slog::info!(ctx.log, "ECAM: malformed access"; 
                        "cfg_offset" => cfg_offset,
                        "len" => rwo.len());
            if let RWOp::Read(ro) = rwo {
                ro.fill(0xff);
            }
            return;
        }

        // Return all set bits for reads from absent devices (PCI local bus spec
        // rev 3.0 SS6).
        let dev = self.device_at(bdf);
        if dev.is_none() {
            slog::info!(ctx.log, "ECAM access: device not found, ignoring");
            if let RWOp::Read(ro) = rwo {
                ro.fill(0xff);
            }
            return;
        }

        // The device is present, so let it handle the read or write.
        let dev = dev.unwrap();
        match rwo {
            RWOp::Read(ro) => {
                let mut cro = ReadOp::new_child(cfg_offset, ro, ..);
                dev.cfg_rw(RWOp::Read(&mut cro), ctx);
            }
            RWOp::Write(wo) => {
                let mut cro = WriteOp::new_child(cfg_offset, wo, ..);
                dev.cfg_rw(RWOp::Write(&mut cro), ctx);
            }
        }
    }
}

#[cfg(test)]
mod test {
    use std::convert::TryInto;

    use crate::{
        common::ReadOp,
        hw::pci::bits::{ADDR_ECAM_REGION_BASE, LEN_CFG},
        instance::Instance,
    };

    use super::*;

    fn prep() -> (Arc<PioBus>, Arc<MmioBus>) {
        (Arc::new(PioBus::new()), Arc::new(MmioBus::new(u32::MAX as usize)))
    }

    struct TestDev {
        inner: Mutex<Option<Attachment>>,
        cfg_data: Mutex<[u8; LEN_CFG]>,
    }
    impl Default for TestDev {
        fn default() -> Self {
            Self {
                inner: Default::default(),
                cfg_data: Mutex::new([0u8; LEN_CFG]),
            }
        }
    }
    impl Endpoint for TestDev {
        fn attach(&self, attachment: Attachment) {
            let mut attach = self.inner.lock().unwrap();
            attach.replace(attachment);
        }
        fn cfg_rw(&self, op: RWOp, _ctx: &DispCtx) {
            match op {
                RWOp::Read(ro) => {
                    let cfg_data = self.cfg_data.lock().unwrap();
                    ro.write_bytes(
                        &cfg_data[std::ops::Range {
                            start: ro.offset(),
                            end: ro.offset() + ro.len(),
                        }],
                    );
                }
                _ => {}
            }
        }
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

    struct TestEnv {
        bus: Bus,
        devs: Vec<Arc<TestDev>>,
        instance: Arc<Instance>,
    }

    impl TestEnv {
        fn new() -> Self {
            let (pio, mmio) = prep();
            Self {
                bus: Bus::new(BusNum::new(0).unwrap(), &pio, &mmio),
                devs: vec![],
                instance: Instance::new_test(None).unwrap(),
            }
        }

        fn with_devices(bdfs: &Vec<Bdf>) -> Self {
            let mut this = Self::new();
            for bdf in bdfs {
                this.devs.push(Arc::new(TestDev::default()));
                this.bus.attach(
                    *bdf,
                    Arc::clone(this.devs.last().unwrap()) as Arc<dyn Endpoint>,
                    None,
                );
            }
            this
        }
    }

    // Verify that reads directed to different portions of a device's config
    // space return values from the target portion.
    #[test]
    fn ecam_read_at_offset() {
        let env = TestEnv::with_devices(&vec![Bdf::new(0, 0, 0).unwrap()]);
        let mut data: Vec<u8> = Vec::with_capacity(LEN_CFG);
        for val in 0..LEN_CFG {
            data.push(val as u8);
        }
        *env.devs[0].cfg_data.lock().unwrap() =
            data.as_slice().try_into().unwrap();

        let mut buf = [0xffu8; 4];
        let mut read_op = ReadOp::from_buf(0, &mut buf);
        env.instance.disp.with_ctx(|ctx| {
            env.bus.extended_cfg_rw(
                ADDR_ECAM_REGION_BASE,
                RWOp::Read(&mut read_op),
                ctx,
            );
        });
        assert_eq!(buf, [0, 1, 2, 3]);

        let mut buf = [0xffu8; 4];
        let mut read_op = ReadOp::from_buf(8, &mut buf);
        env.instance.disp.with_ctx(|ctx| {
            env.bus.extended_cfg_rw(
                ADDR_ECAM_REGION_BASE,
                RWOp::Read(&mut read_op),
                ctx,
            );
        });
        assert_eq!(buf, [8, 9, 10, 11]);
    }

    // Verify that reads of different devices/functions on the same bus are
    // routed to the correct device.
    #[test]
    fn ecam_targets_devices_within_bus() {
        let bdfs = vec![
            Bdf::new(0, 0, 0).unwrap(),
            Bdf::new(0, 1, 0).unwrap(),
            Bdf::new(0, 1, 1).unwrap(),
        ];

        let env = TestEnv::with_devices(&bdfs);
        *env.devs[0].cfg_data.lock().unwrap() = [0xa0u8; LEN_CFG];
        *env.devs[1].cfg_data.lock().unwrap() = [0xb1u8; LEN_CFG];
        *env.devs[2].cfg_data.lock().unwrap() = [0xc2u8; LEN_CFG];

        let mut buf = [0u8; 4];
        let mut read_op = ReadOp::from_buf(0, &mut buf);
        env.instance.disp.with_ctx(|ctx| {
            env.bus.extended_cfg_rw(
                ADDR_ECAM_REGION_BASE,
                RWOp::Read(&mut read_op),
                ctx,
            );
        });
        assert_eq!(buf, [0xa0u8; 4]);

        buf = [0u8; 4];
        read_op = ReadOp::from_buf(0x8000, &mut buf);
        env.instance.disp.with_ctx(|ctx| {
            env.bus.extended_cfg_rw(
                ADDR_ECAM_REGION_BASE,
                RWOp::Read(&mut read_op),
                ctx,
            );
        });
        assert_eq!(buf, [0xb1u8; 4]);

        buf = [0u8; 4];
        read_op = ReadOp::from_buf(0x9000, &mut buf);
        env.instance.disp.with_ctx(|ctx| {
            env.bus.extended_cfg_rw(
                ADDR_ECAM_REGION_BASE,
                RWOp::Read(&mut read_op),
                ctx,
            );
        });
        assert_eq!(buf, [0xc2u8; 4]);
    }

    // Verify that reads that span multiple doublewords return -1 without
    // panicking.
    #[test]
    fn ecam_cross_alignment_boundary() {
        let env = TestEnv::with_devices(&vec![Bdf::new(0, 0, 0).unwrap()]);
        let mut buf = [0u8; 4];
        let mut read_op = ReadOp::from_buf(1, &mut buf);
        env.instance.disp.with_ctx(|ctx| {
            env.bus.extended_cfg_rw(
                ADDR_ECAM_REGION_BASE,
                RWOp::Read(&mut read_op),
                ctx,
            )
        });
        assert_eq!(buf, [0xffu8; 4]);
    }

    // Verify that unaligned accesses that don't span multiple doublewords
    // return the expected portion of the config space.
    #[test]
    fn ecam_unaligned_within_word() {
        let env = TestEnv::with_devices(&vec![Bdf::new(0, 0, 0).unwrap()]);
        let mut buf = [0xffu8; 2];
        let mut read_op = ReadOp::from_buf(1, &mut buf);
        env.instance.disp.with_ctx(|ctx| {
            env.bus.extended_cfg_rw(
                ADDR_ECAM_REGION_BASE,
                RWOp::Read(&mut read_op),
                ctx,
            )
        });
        assert_eq!(buf, [0u8; 2]);
    }

    // Verify that reads that target an absent device/function return -1 without
    // panicking.
    #[test]
    fn ecam_absent_device() {
        let env = TestEnv::with_devices(&vec![Bdf::new(0, 0, 0).unwrap()]);
        let mut buf = [0u8; 4];
        let mut read_op = ReadOp::from_buf(0xf000, &mut buf);
        env.instance.disp.with_ctx(|ctx| {
            env.bus.extended_cfg_rw(
                ADDR_ECAM_REGION_BASE,
                RWOp::Read(&mut read_op),
                ctx,
            )
        });
        assert_eq!(buf, [0xffu8; 4]);
    }
}
