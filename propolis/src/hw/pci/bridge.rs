//! Support for PCI bridges.

use std::sync::{Arc, Mutex};

use super::bits::{HEADER_TYPE_BRIDGE, LEN_CFG_STD};
use super::router::Router;
use super::{BarN, Builder, Bus, BusNum, Device, DeviceState, StdCfgReg};
use crate::common::{RWOp, ReadOp, WriteOp};
use crate::dispatch::DispCtx;
use crate::inventory::Entity;
use crate::migrate::Migrator;
use crate::util::regmap::RegMap;

use lazy_static::lazy_static;

// Class code identifiers required by SS3.2.4.6 of the PCI bridge spec rev 1.2.
const BRIDGE_PROG_CLASS: u8 = 0x06;
const BRIDGE_PROG_SUBCLASS: u8 = 0x04;
const BRIDGE_PROG_IF: u8 = 0x00;

// Clear all reserved bits and decline to emulate error reporting bits in the
// bridge secondary status register (SS3.2.5.7).
const BRIDGE_SECONDARY_STATUS: u16 = 0x0000;

// Mask for the reserved bottom bits of the memory base and memory limit
// registers (SS3.2.5.8).
const BRIDGE_MEMORY_REG_MASK: u16 = 0xfff0;

// Bridge configuration space header registers.
#[derive(Copy, Clone, Eq, PartialEq, Debug)]
enum CfgReg {
    Common(StdCfgReg),
    PrimaryBus,
    SecondaryBus,
    SubordinateBus,
    SecondaryLatencyTimer,
    IoBase,
    IoLimit,
    SecondaryStatus,
    MemoryBase,
    MemoryLimit,
    PrefetchableMemoryBase,
    PrefetchableMemoryLimit,
    PrefetchableMemoryBaseUpper,
    PrefetchableMemoryLimitUpper,
    IoBaseUpper,
    IoLimitUpper,
    BridgeControl,
}

lazy_static! {
    static ref CFG_HEADER_MAP: RegMap<CfgReg> = {
        let layout = [
            (CfgReg::Common(StdCfgReg::VendorId), 2),
            (CfgReg::Common(StdCfgReg::DeviceId), 2),
            (CfgReg::Common(StdCfgReg::Command), 2),
            (CfgReg::Common(StdCfgReg::Status), 2),
            (CfgReg::Common(StdCfgReg::RevisionId), 1),
            (CfgReg::Common(StdCfgReg::ProgIf), 1),
            (CfgReg::Common(StdCfgReg::Subclass), 1),
            (CfgReg::Common(StdCfgReg::Class), 1),
            (CfgReg::Common(StdCfgReg::CacheLineSize), 1),
            (CfgReg::Common(StdCfgReg::LatencyTimer), 1),
            (CfgReg::Common(StdCfgReg::HeaderType), 1),
            (CfgReg::Common(StdCfgReg::Bist), 1),
            (CfgReg::Common(StdCfgReg::Bar(BarN::BAR0)), 4),
            (CfgReg::Common(StdCfgReg::Bar(BarN::BAR1)), 4),
            (CfgReg::PrimaryBus, 1),
            (CfgReg::SecondaryBus, 1),
            (CfgReg::SubordinateBus, 1),
            (CfgReg::SecondaryLatencyTimer, 1),
            (CfgReg::IoBase, 1),
            (CfgReg::IoLimit, 1),
            (CfgReg::SecondaryStatus, 2),
            (CfgReg::MemoryBase, 2),
            (CfgReg::MemoryLimit, 2),
            (CfgReg::PrefetchableMemoryBase, 2),
            (CfgReg::PrefetchableMemoryLimit, 2),
            (CfgReg::PrefetchableMemoryBaseUpper, 4),
            (CfgReg::PrefetchableMemoryLimitUpper, 4),
            (CfgReg::IoBaseUpper, 2),
            (CfgReg::IoLimitUpper, 2),
            (CfgReg::Common(StdCfgReg::CapPtr), 1),
            (CfgReg::Common(StdCfgReg::Reserved), 3),
            (CfgReg::Common(StdCfgReg::ExpansionRomAddr), 4),
            (CfgReg::Common(StdCfgReg::IntrLine), 1),
            (CfgReg::Common(StdCfgReg::IntrPin), 1),
            (CfgReg::BridgeControl, 2),
        ];
        RegMap::create_packed(
            LEN_CFG_STD,
            &layout,
            Some(CfgReg::Common(StdCfgReg::Reserved)),
        )
    };
}

pub struct Bridge {
    pci_state: DeviceState,
    inner: Mutex<Inner>,
}

impl Bridge {
    pub fn new(bus: Arc<Bus>, router: Arc<Router>) -> Arc<Self> {
        let builder = Builder::new(super::Ident {
            vendor_id: 0x1de,
            prog_if: BRIDGE_PROG_IF,
            subclass: BRIDGE_PROG_SUBCLASS,
            class: BRIDGE_PROG_CLASS,
            ..Default::default()
        });
        Arc::new(Self {
            pci_state: builder.finish(),
            inner: Mutex::new(Inner::new(bus, router)),
        })
    }
}

impl Device for Bridge {
    fn device_state(&self) -> &DeviceState {
        &self.pci_state
    }

    fn std_cfg_rw(&self, mut rwo: RWOp, ctx: &DispCtx) {
        CFG_HEADER_MAP.process(&mut rwo, |id, rwo| match rwo {
            RWOp::Read(ro) => match id {
                CfgReg::Common(StdCfgReg::HeaderType) => {
                    ro.write_u8(HEADER_TYPE_BRIDGE)
                }
                CfgReg::Common(id) => self.pci_state.cfg_std_read(id, ro, ctx),
                _ => self.inner.lock().unwrap().cfg_std_read(id, ro, ctx),
            },
            RWOp::Write(wo) => match id {
                CfgReg::Common(id) => {
                    self.pci_state.cfg_std_write(self, id, wo, ctx)
                }
                _ => self.inner.lock().unwrap().cfg_std_write(id, wo, ctx),
            },
        })
    }
}

impl Entity for Bridge {
    fn type_name(&self) -> &'static str {
        "pci-bridge"
    }
    fn reset(&self, _ctx: &DispCtx) {
        self.inner.lock().unwrap().reset();
    }
    fn migrate(&self) -> Migrator {
        // XXX Should be migratable in theory.
        Migrator::NonMigratable
    }
}

struct Inner {
    bus: Arc<Bus>,
    router: Arc<Router>,
    primary_bus: BusNum,
    secondary_bus: BusNum,
    subordinate_bus: BusNum,
    memory_base: u16,
    memory_limit: u16,
}

impl Inner {
    fn new(bus: Arc<Bus>, router: Arc<Router>) -> Self {
        Self {
            bus,
            router,
            primary_bus: BusNum::new(0).unwrap(),
            secondary_bus: BusNum::new(0).unwrap(),
            subordinate_bus: BusNum::new(0).unwrap(),
            memory_base: 0,
            memory_limit: 0,
        }
    }

    fn cfg_std_read(&self, id: &CfgReg, ro: &mut ReadOp, _ctx: &DispCtx) {
        match id {
            CfgReg::PrimaryBus => ro.write_u8(self.primary_bus.get()),
            CfgReg::SecondaryBus => ro.write_u8(self.secondary_bus.get()),
            CfgReg::SubordinateBus => ro.write_u8(self.subordinate_bus.get()),
            CfgReg::SecondaryLatencyTimer => ro.write_u8(0),
            CfgReg::IoBase | CfgReg::IoLimit => ro.write_u8(0),
            CfgReg::SecondaryStatus => ro.write_u16(BRIDGE_SECONDARY_STATUS),
            CfgReg::MemoryBase => {
                ro.write_u16(self.memory_base & BRIDGE_MEMORY_REG_MASK)
            }
            CfgReg::MemoryLimit => {
                ro.write_u16(self.memory_limit & BRIDGE_MEMORY_REG_MASK)
            }
            CfgReg::PrefetchableMemoryBase
            | CfgReg::PrefetchableMemoryLimit => ro.write_u16(0),
            CfgReg::PrefetchableMemoryBaseUpper
            | CfgReg::PrefetchableMemoryLimitUpper => ro.write_u32(0),
            CfgReg::IoBaseUpper | CfgReg::IoLimitUpper => ro.write_u16(0),
            CfgReg::BridgeControl => ro.write_u16(0),
            CfgReg::Common(_) => {
                panic!("Common register read in bridge header not delegated")
            }
        }
    }

    fn cfg_std_write(&mut self, id: &CfgReg, wo: &mut WriteOp, _ctx: &DispCtx) {
        match id {
            CfgReg::Common(_) => {
                panic!("Common register write in bridge header not delegated")
            }
            CfgReg::PrimaryBus => {
                self.primary_bus = BusNum::new(wo.read_u8()).unwrap();
            }
            CfgReg::SecondaryBus => {
                self.set_secondary_bus(BusNum::new(wo.read_u8()).unwrap());
            }
            CfgReg::SubordinateBus => {
                self.subordinate_bus = BusNum::new(wo.read_u8()).unwrap();
            }
            CfgReg::MemoryBase => {
                self.memory_base = wo.read_u16();
            }
            CfgReg::MemoryLimit => {
                self.memory_limit = wo.read_u16();
            }
            _ => {}
        }
    }

    fn set_secondary_bus(&mut self, n: BusNum) {
        if self.secondary_bus.get() != 0 {
            self.router.set(self.secondary_bus, None);
        }
        self.secondary_bus = n;
        if n.get() != 0 {
            self.router.set(self.secondary_bus, Some(self.bus.clone()));
        }
    }

    fn reset(&mut self) {
        self.primary_bus = BusNum::new(0).unwrap();
        self.set_secondary_bus(BusNum::new(0).unwrap());
        self.subordinate_bus = BusNum::new(0).unwrap();
        self.memory_base = 0;
        self.memory_limit = 0;
    }
}

#[cfg(test)]
mod test {
    use crate::hw::pci::Endpoint;
    use crate::instance::Instance;
    use crate::mmio::MmioBus;
    use crate::pio::PioBus;

    use super::*;

    const OFFSET_PRIMARY_BUS: usize = 0x18;
    const OFFSET_SECONDARY_BUS: usize = 0x19;
    const OFFSET_SUBORDINATE_BUS: usize = 0x20;

    struct Env {
        instance: Arc<Instance>,
        router: Arc<Router>,
        pio: Arc<PioBus>,
        mmio: Arc<MmioBus>,
    }

    impl Env {
        fn new() -> Self {
            Self {
                instance: Instance::new_test(None).unwrap(),
                router: Arc::new(Router::default()),
                pio: Arc::new(PioBus::new()),
                mmio: Arc::new(MmioBus::new(u32::MAX as usize)),
            }
        }

        fn make_bus(&self, n: BusNum) -> Arc<Bus> {
            Arc::new(Bus::new(n, &self.pio, &self.mmio))
        }
    }

    #[test]
    fn bridge_header_type() {
        let env = Env::new();
        let bridge = Bridge::new(
            env.make_bus(BusNum::new(0).unwrap()),
            env.router.clone(),
        );
        let mut buf = [0xffu8; 1];
        let mut ro = ReadOp::from_buf(0xe, &mut buf);
        env.instance.disp.with_ctx(|ctx| {
            Endpoint::cfg_rw(bridge.as_ref(), RWOp::Read(&mut ro), ctx);
        });
        assert_eq!(buf[0], HEADER_TYPE_BRIDGE);
    }

    #[test]
    fn bridge_bus_registers() {
        let env = Env::new();
        let bridge = Bridge::new(
            env.make_bus(BusNum::new(0).unwrap()),
            env.router.clone(),
        );
        let vals: Vec<u8> = vec![
            OFFSET_PRIMARY_BUS as u8,
            OFFSET_SECONDARY_BUS as u8,
            OFFSET_SUBORDINATE_BUS as u8,
        ];
        for val in &vals {
            let mut buf = [*val; 1];
            let mut wo = WriteOp::from_buf(*val as usize, &mut buf);
            env.instance.disp.with_ctx(|ctx| {
                Endpoint::cfg_rw(bridge.as_ref(), RWOp::Write(&mut wo), ctx);
            });
        }
        for val in &vals {
            let mut buf = [0u8; 1];
            let mut ro = ReadOp::from_buf(*val as usize, &mut buf);
            env.instance.disp.with_ctx(|ctx| {
                Endpoint::cfg_rw(bridge.as_ref(), RWOp::Read(&mut ro), ctx);
            });
            assert_eq!(buf[0], *val);
        }
    }

    #[test]
    fn bridge_routing() {
        let env = Env::new();
        let bus = env.make_bus(BusNum::new(1).unwrap());
        let bridge = Bridge::new(bus.clone(), env.router.clone());

        let mut buf = [42u8; 1];
        let mut wo = WriteOp::from_buf(OFFSET_SECONDARY_BUS, &mut buf);
        env.instance.disp.with_ctx(|ctx| {
            Endpoint::cfg_rw(bridge.as_ref(), RWOp::Write(&mut wo), ctx);
        });
        assert!(Arc::ptr_eq(
            &bus,
            &env.router.get(BusNum::new(42).unwrap()).unwrap()
        ));

        buf[0] = 0;
        let mut wo = WriteOp::from_buf(OFFSET_SECONDARY_BUS, &mut buf);
        env.instance.disp.with_ctx(|ctx| {
            Endpoint::cfg_rw(bridge.as_ref(), RWOp::Write(&mut wo), ctx);
        });
        assert!(env.router.get(BusNum::new(42).unwrap()).is_none());

        let bus2 = env.make_bus(BusNum::new(2).unwrap());
        let bridge2 = Bridge::new(bus2.clone(), env.router.clone());
        buf[0] = 42;
        let mut wo = WriteOp::from_buf(OFFSET_SECONDARY_BUS, &mut buf);
        env.instance.disp.with_ctx(|ctx| {
            Endpoint::cfg_rw(bridge2.as_ref(), RWOp::Write(&mut wo), ctx);
        });
        assert!(Arc::ptr_eq(
            &bus2,
            &env.router.get(BusNum::new(42).unwrap()).unwrap()
        ));

        buf[0] = 1;
        let mut wo = WriteOp::from_buf(OFFSET_SECONDARY_BUS, &mut buf);
        env.instance.disp.with_ctx(|ctx| {
            Endpoint::cfg_rw(bridge.as_ref(), RWOp::Write(&mut wo), ctx);
        });
        assert!(Arc::ptr_eq(
            &bus,
            &env.router.get(BusNum::new(1).unwrap()).unwrap()
        ));
        assert!(Arc::ptr_eq(
            &bus2,
            &env.router.get(BusNum::new(42).unwrap()).unwrap()
        ));
        assert!(!Arc::ptr_eq(
            &env.router.get(BusNum::new(1).unwrap()).unwrap(),
            &env.router.get(BusNum::new(42).unwrap()).unwrap()
        ));

        buf[0] = 0;
        let mut wo = WriteOp::from_buf(OFFSET_SECONDARY_BUS, &mut buf);
        env.instance.disp.with_ctx(|ctx| {
            Endpoint::cfg_rw(bridge2.as_ref(), RWOp::Write(&mut wo), ctx);
        });
        assert!(env.router.get(BusNum::new(0).unwrap()).is_none());
        assert!(env.router.get(BusNum::new(42).unwrap()).is_none());
    }
}
