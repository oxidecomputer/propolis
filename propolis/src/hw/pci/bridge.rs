//! Support for PCI bridges.

use std::num::NonZeroU8;
use std::sync::{Arc, Mutex};

use super::bus::Attachment;
use super::cfgspace::{CfgBuilder, CfgReg};
use super::router::Router;
use super::{bits::*, Endpoint};
use super::{BarN, Bus, BusNum, StdCfgReg};
use crate::common::{RWOp, ReadOp, WriteOp};
use crate::dispatch::DispCtx;
use crate::inventory::Entity;
use crate::migrate::Migrator;
use crate::util::regmap::RegMap;

use lazy_static::lazy_static;

// Bridge configuration space header registers.
#[derive(Copy, Clone, Eq, PartialEq, Debug)]
enum BridgeReg {
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
    static ref CFG_HEADER_MAP: RegMap<BridgeReg> = {
        let layout = [
            (BridgeReg::Common(StdCfgReg::VendorId), 2),
            (BridgeReg::Common(StdCfgReg::DeviceId), 2),
            (BridgeReg::Common(StdCfgReg::Command), 2),
            (BridgeReg::Common(StdCfgReg::Status), 2),
            (BridgeReg::Common(StdCfgReg::RevisionId), 1),
            (BridgeReg::Common(StdCfgReg::ProgIf), 1),
            (BridgeReg::Common(StdCfgReg::Subclass), 1),
            (BridgeReg::Common(StdCfgReg::Class), 1),
            (BridgeReg::Common(StdCfgReg::CacheLineSize), 1),
            (BridgeReg::Common(StdCfgReg::LatencyTimer), 1),
            (BridgeReg::Common(StdCfgReg::HeaderType), 1),
            (BridgeReg::Common(StdCfgReg::Bist), 1),
            (BridgeReg::Common(StdCfgReg::Bar(BarN::BAR0)), 4),
            (BridgeReg::Common(StdCfgReg::Bar(BarN::BAR1)), 4),
            (BridgeReg::PrimaryBus, 1),
            (BridgeReg::SecondaryBus, 1),
            (BridgeReg::SubordinateBus, 1),
            (BridgeReg::SecondaryLatencyTimer, 1),
            (BridgeReg::IoBase, 1),
            (BridgeReg::IoLimit, 1),
            (BridgeReg::SecondaryStatus, 2),
            (BridgeReg::MemoryBase, 2),
            (BridgeReg::MemoryLimit, 2),
            (BridgeReg::PrefetchableMemoryBase, 2),
            (BridgeReg::PrefetchableMemoryLimit, 2),
            (BridgeReg::PrefetchableMemoryBaseUpper, 4),
            (BridgeReg::PrefetchableMemoryLimitUpper, 4),
            (BridgeReg::IoBaseUpper, 2),
            (BridgeReg::IoLimitUpper, 2),
            (BridgeReg::Common(StdCfgReg::CapPtr), 1),
            (BridgeReg::Common(StdCfgReg::Reserved), 3),
            (BridgeReg::Common(StdCfgReg::ExpansionRomAddr), 4),
            (BridgeReg::Common(StdCfgReg::IntrLine), 1),
            (BridgeReg::Common(StdCfgReg::IntrPin), 1),
            (BridgeReg::BridgeControl, 2),
        ];
        RegMap::create_packed(
            LEN_CFG_STD,
            &layout,
            Some(BridgeReg::Common(StdCfgReg::Reserved)),
        )
    };
}

/// A PCI-PCI bridge.
pub struct Bridge {
    // The common PCI state has its own synchronization. Accesses to it are
    // currently mutually exclusive with accesses to the bridge state (i.e. no
    // single config transaction is expected to access both common state and
    // bridge state).
    cfg_map: RegMap<CfgReg>,
    inner: Mutex<Inner>,
}

impl Bridge {
    /// Construct a new PCI bridge with the supplied downstream bus. Updating
    /// the bridge's secondary bus number will update the supplied router such
    /// that it maps the new bus number to the bridge's downstream bus.
    pub fn new(bus: Arc<Bus>, router: Arc<Router>) -> Arc<Self> {
        let cfg_builder = CfgBuilder::new();
        Arc::new(Self {
            cfg_map: cfg_builder.finish().0,
            inner: Mutex::new(Inner::new(bus, router)),
        })
    }

    fn cfg_header_rw(&self, mut rwo: RWOp, ctx: &DispCtx) {
        CFG_HEADER_MAP.process(&mut rwo, |id, rwo| match rwo {
            RWOp::Read(ro) => {
                self.cfg_std_read(id, ro, ctx);
            }
            RWOp::Write(wo) => {
                self.cfg_std_write(id, wo, ctx);
            }
        })
    }

    fn cfg_std_read(&self, id: &BridgeReg, ro: &mut ReadOp, _ctx: &DispCtx) {
        match id {
            BridgeReg::Common(id) => match id {
                StdCfgReg::VendorId => ro.write_u16(BRIDGE_VENDOR_ID),
                StdCfgReg::DeviceId => ro.write_u16(BRIDGE_DEVICE_ID),
                StdCfgReg::Class => ro.write_u8(BRIDGE_PROG_CLASS),
                StdCfgReg::Subclass => ro.write_u8(BRIDGE_PROG_SUBCLASS),
                StdCfgReg::SubVendorId => ro.write_u16(BRIDGE_VENDOR_ID),
                StdCfgReg::SubDeviceId => ro.write_u16(BRIDGE_DEVICE_ID),
                StdCfgReg::ProgIf => ro.write_u8(BRIDGE_PROG_IF),
                StdCfgReg::RevisionId => ro.write_u8(0),
                StdCfgReg::HeaderType => ro.write_u8(HEADER_TYPE_BRIDGE),
                StdCfgReg::Reserved => ro.fill(0),
                StdCfgReg::Command => {
                    let guard = self.inner.lock().unwrap();
                    ro.write_u16(guard.reg_command.bits());
                }

                // The bridge never generates its own interrupts and currently
                // has no capabilities, so set both of those bits to 0.
                StdCfgReg::Status => ro.write_u16(0),

                // Disable interrupts from the bridge device itself (SS3.2.5.16
                // and 17).
                StdCfgReg::IntrLine => ro.write_u8(0xFF),
                StdCfgReg::IntrPin => ro.write_u8(0),

                // The bridge has no internal resources, so disable its BARs.
                // This doesn't affect transactions that cross the bridge
                // (SS3.2.5.1).
                StdCfgReg::Bar(_) => ro.write_u32(0),

                // Expansion ROMs are not supported.
                StdCfgReg::ExpansionRomAddr => ro.write_u32(0),

                // No capabilities for now.
                StdCfgReg::CapPtr => ro.write_u8(0),

                // Other registers defined to be optional in SS3.2.4.
                StdCfgReg::CacheLineSize => ro.write_u8(0),
                StdCfgReg::LatencyTimer => ro.write_u8(0),
                StdCfgReg::Bist => ro.write_u8(0),

                // These registers appear in type-0 PCI headers, but not bridge
                // headers.
                StdCfgReg::MaxLatency
                | StdCfgReg::MinGrant
                | StdCfgReg::CardbusPtr => {
                    panic!("Unexpected register type {:?}", id);
                }
            },
            BridgeReg::PrimaryBus => {
                let guard = self.inner.lock().unwrap();
                ro.write_u8(guard.primary_bus.get());
            }
            BridgeReg::SecondaryBus => {
                let guard = self.inner.lock().unwrap();
                ro.write_u8(guard.secondary_bus.get());
            }
            BridgeReg::SubordinateBus => {
                let guard = self.inner.lock().unwrap();
                ro.write_u8(guard.subordinate_bus.get());
            }
            BridgeReg::SecondaryLatencyTimer => ro.write_u8(0),
            BridgeReg::IoBase | BridgeReg::IoLimit => ro.write_u8(0),
            BridgeReg::SecondaryStatus => ro.write_u16(BRIDGE_SECONDARY_STATUS),
            BridgeReg::MemoryBase => {
                let guard = self.inner.lock().unwrap();
                ro.write_u16(guard.memory_base & BRIDGE_MEMORY_REG_MASK);
            }
            BridgeReg::MemoryLimit => {
                let guard = self.inner.lock().unwrap();
                ro.write_u16(guard.memory_limit & BRIDGE_MEMORY_REG_MASK);
            }
            BridgeReg::PrefetchableMemoryBase
            | BridgeReg::PrefetchableMemoryLimit => ro.write_u16(0),
            BridgeReg::PrefetchableMemoryBaseUpper
            | BridgeReg::PrefetchableMemoryLimitUpper => ro.write_u32(0),
            BridgeReg::IoBaseUpper | BridgeReg::IoLimitUpper => ro.write_u16(0),
            BridgeReg::BridgeControl => ro.write_u16(0),
        }
    }

    fn cfg_std_write(&self, id: &BridgeReg, wo: &mut WriteOp, _ctx: &DispCtx) {
        match id {
            BridgeReg::Common(id) => match id {
                // Ignore writes to read-only standard registers.
                StdCfgReg::VendorId
                | StdCfgReg::DeviceId
                | StdCfgReg::Class
                | StdCfgReg::Subclass
                | StdCfgReg::SubVendorId
                | StdCfgReg::SubDeviceId
                | StdCfgReg::ProgIf
                | StdCfgReg::RevisionId
                | StdCfgReg::HeaderType
                | StdCfgReg::CapPtr
                | StdCfgReg::CacheLineSize
                | StdCfgReg::LatencyTimer
                | StdCfgReg::Bist
                | StdCfgReg::Reserved => {}

                StdCfgReg::Command => {
                    let new = RegCmd::from_bits_truncate(wo.read_u16());
                    let mut guard = self.inner.lock().unwrap();
                    guard.reg_command = new;
                }

                // Writes to the status register are supported to clear certain
                // error bits, but the virtual bridge doesn't report these
                // errors, so just ignore writes to this register.
                StdCfgReg::Status => {}

                // The bridge has no interrupt signal pin, so treat its line
                // register as read-only.
                StdCfgReg::IntrLine | StdCfgReg::IntrPin => {}

                // The bridge has no internal resources, so disable its BARs.
                // This doesn't affect transactions that cross the bridge
                // (SS3.2.5.1).
                StdCfgReg::Bar(_) => {}

                // Expansion ROMs are not supported.
                StdCfgReg::ExpansionRomAddr => {}

                // These registers appear in type-0 PCI headers, but not bridge
                // headers.
                StdCfgReg::MaxLatency
                | StdCfgReg::MinGrant
                | StdCfgReg::CardbusPtr => {
                    panic!("Unexpected register type {:?}", id);
                }
            },

            // Writable bridge registers.
            BridgeReg::PrimaryBus => {
                let mut guard = self.inner.lock().unwrap();
                guard.primary_bus = BusNum::new(wo.read_u8()).unwrap()
            }
            BridgeReg::SecondaryBus => {
                let mut guard = self.inner.lock().unwrap();
                guard.set_secondary_bus(BusNum::new(wo.read_u8()).unwrap())
            }
            BridgeReg::SubordinateBus => {
                let mut guard = self.inner.lock().unwrap();
                guard.subordinate_bus = BusNum::new(wo.read_u8()).unwrap()
            }
            BridgeReg::MemoryBase => {
                let mut guard = self.inner.lock().unwrap();
                guard.memory_base = wo.read_u16();
            }
            BridgeReg::MemoryLimit => {
                let mut guard = self.inner.lock().unwrap();
                guard.memory_limit = wo.read_u16();
            }

            // Read-only bridge registers.
            BridgeReg::SecondaryLatencyTimer => {}
            BridgeReg::IoBase | BridgeReg::IoLimit => {}
            BridgeReg::SecondaryStatus => {}
            BridgeReg::PrefetchableMemoryBase
            | BridgeReg::PrefetchableMemoryLimit => {}
            BridgeReg::PrefetchableMemoryBaseUpper
            | BridgeReg::PrefetchableMemoryLimitUpper => {}
            BridgeReg::IoBaseUpper | BridgeReg::IoLimitUpper => {}
            BridgeReg::BridgeControl => {}
        }
    }
}

impl Endpoint for Bridge {
    fn attach(&self, attachment: Attachment) {
        let mut inner = self.inner.lock().unwrap();
        let _old = inner.attachment.replace(attachment);
        assert!(_old.is_none());
    }

    fn cfg_rw(&self, mut rwo: RWOp, ctx: &DispCtx) {
        self.cfg_map.process(&mut rwo, |id, rwo| match id {
            CfgReg::Std => {
                self.cfg_header_rw(rwo, ctx);
            }
            _ => {
                panic!(
                    "Unexpected read of bridge config space with ID {:?}",
                    id
                )
            }
        });
    }

    fn bar_rw(&self, _bar: BarN, _rwo: RWOp, _ctx: &DispCtx) {
        // The BARs in the PCI bridge are read-only, so nothing should ever
        // try to dispatch an I/O to a region defined in a bridge's BAR.
        panic!("unexpected BAR read/write in PCI bridge");
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
        // TODO Should be migratable in theory: copy all the register state,
        // then enumerate bridges on the target and reconstruct the routing
        // table from their bus registers' values.
        Migrator::NonMigratable
    }
}

struct Inner {
    attachment: Option<Attachment>,

    bus: Arc<Bus>,
    router: Arc<Router>,

    reg_command: RegCmd,
    primary_bus: BusNum,
    secondary_bus: BusNum,
    subordinate_bus: BusNum,
    memory_base: u16,
    memory_limit: u16,
}

impl Inner {
    fn new(bus: Arc<Bus>, router: Arc<Router>) -> Self {
        Self {
            attachment: None,
            bus,
            router,
            reg_command: RegCmd::empty(),
            primary_bus: BusNum::new(0).unwrap(),
            secondary_bus: BusNum::new(0).unwrap(),
            subordinate_bus: BusNum::new(0).unwrap(),
            memory_base: 0,
            memory_limit: 0,
        }
    }

    fn set_secondary_bus(&mut self, n: BusNum) {
        if let Some(bus) = NonZeroU8::new(self.secondary_bus.get()) {
            self.router.set(bus, None)
        }
        self.secondary_bus = n;
        if let Some(bus) = NonZeroU8::new(self.secondary_bus.get()) {
            self.router.set(bus, Some(self.bus.clone()));
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

        fn make_bus(&self) -> Arc<Bus> {
            Arc::new(Bus::new(&self.pio, &self.mmio))
        }
    }

    #[test]
    fn bridge_header_type() {
        let env = Env::new();
        let bridge = Bridge::new(env.make_bus(), env.router.clone());
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
        let bridge = Bridge::new(env.make_bus(), env.router.clone());

        // Write the offsets of the primary, secondary, and subordinate bus
        // registers to those registers, then verify that they can be read
        // back.
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
        let bus = env.make_bus();
        let bridge = Bridge::new(bus.clone(), env.router.clone());

        // Write 42 to the test bridge's secondary bus register and verify that
        // this bus number routes to the downstream bus.
        let mut buf = [42u8; 1];
        let mut wo = WriteOp::from_buf(OFFSET_SECONDARY_BUS, &mut buf);
        env.instance.disp.with_ctx(|ctx| {
            Endpoint::cfg_rw(bridge.as_ref(), RWOp::Write(&mut wo), ctx);
        });
        assert!(Arc::ptr_eq(
            &bus,
            &env.router.get(NonZeroU8::new(42).unwrap()).unwrap()
        ));

        // Clear the test bridge's secondary bus register and verify the routing
        // is removed.
        buf[0] = 0;
        let mut wo = WriteOp::from_buf(OFFSET_SECONDARY_BUS, &mut buf);
        env.instance.disp.with_ctx(|ctx| {
            Endpoint::cfg_rw(bridge.as_ref(), RWOp::Write(&mut wo), ctx);
        });
        assert!(env.router.get(NonZeroU8::new(42).unwrap()).is_none());

        // Route bus number 42 to a new bus.
        let bus2 = env.make_bus();
        let bridge2 = Bridge::new(bus2.clone(), env.router.clone());
        buf[0] = 42;
        let mut wo = WriteOp::from_buf(OFFSET_SECONDARY_BUS, &mut buf);
        env.instance.disp.with_ctx(|ctx| {
            Endpoint::cfg_rw(bridge2.as_ref(), RWOp::Write(&mut wo), ctx);
        });
        assert!(Arc::ptr_eq(
            &bus2,
            &env.router.get(NonZeroU8::new(42).unwrap()).unwrap()
        ));

        // Route bus number 1 to the original bridge's downstream bus. Verify
        // that the routings are correct and distinct from one another.
        buf[0] = 1;
        let mut wo = WriteOp::from_buf(OFFSET_SECONDARY_BUS, &mut buf);
        env.instance.disp.with_ctx(|ctx| {
            Endpoint::cfg_rw(bridge.as_ref(), RWOp::Write(&mut wo), ctx);
        });
        assert!(Arc::ptr_eq(
            &bus,
            &env.router.get(NonZeroU8::new(1).unwrap()).unwrap()
        ));
        assert!(Arc::ptr_eq(
            &bus2,
            &env.router.get(NonZeroU8::new(42).unwrap()).unwrap()
        ));
        assert!(!Arc::ptr_eq(
            &env.router.get(NonZeroU8::new(1).unwrap()).unwrap(),
            &env.router.get(NonZeroU8::new(42).unwrap()).unwrap()
        ));

        // Clear the second bridge's routing and verify that the first bridge's
        // routing is left alone.
        buf[0] = 0;
        let mut wo = WriteOp::from_buf(OFFSET_SECONDARY_BUS, &mut buf);
        env.instance.disp.with_ctx(|ctx| {
            Endpoint::cfg_rw(bridge2.as_ref(), RWOp::Write(&mut wo), ctx);
        });
        assert!(Arc::ptr_eq(
            &bus,
            &env.router.get(NonZeroU8::new(1).unwrap()).unwrap()
        ));
        assert!(env.router.get(NonZeroU8::new(42).unwrap()).is_none());

        // Clear the first bridge's routing and verify that its entry is also
        // safely removed.
        let mut wo = WriteOp::from_buf(OFFSET_SECONDARY_BUS, &mut buf);
        env.instance.disp.with_ctx(|ctx| {
            Endpoint::cfg_rw(bridge.as_ref(), RWOp::Write(&mut wo), ctx);
        });
        assert!(env.router.get(NonZeroU8::new(1).unwrap()).is_none());
        assert!(env.router.get(NonZeroU8::new(42).unwrap()).is_none());
    }
}
