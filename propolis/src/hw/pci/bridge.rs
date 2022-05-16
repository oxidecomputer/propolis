//! Support for PCI bridges.

use std::sync::{Arc, Mutex};

use super::bits::{HEADER_TYPE_BRIDGE, LEN_CFG_STD};
use super::{BarN, Builder, BusNum, Device, DeviceState, StdCfgReg};
use crate::common::{RWOp, ReadOp, WriteOp};
use crate::dispatch::DispCtx;
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
    pub fn new() -> Arc<Self> {
        let builder = Builder::new(super::Ident {
            vendor_id: 0x1de,
            prog_if: BRIDGE_PROG_IF,
            subclass: BRIDGE_PROG_SUBCLASS,
            class: BRIDGE_PROG_CLASS,
            ..Default::default()
        });
        Arc::new(Self {
            pci_state: builder.finish(),
            inner: Mutex::new(Inner::new()),
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

struct Inner {
    primary_bus: BusNum,
    secondary_bus: BusNum,
    subordinate_bus: BusNum,
    memory_base: u16,
    memory_limit: u16,
}

impl Inner {
    fn new() -> Self {
        Self {
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
                // XXX change routing
                self.secondary_bus = BusNum::new(wo.read_u8()).unwrap();
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
}

#[cfg(test)]
mod test {
    use crate::hw::pci::Endpoint;
    use crate::instance::Instance;

    use super::*;

    struct Env {
        instance: Arc<Instance>,
    }

    impl Env {
        fn new() -> Self {
            Self { instance: Instance::new_test(None).unwrap() }
        }
    }

    #[test]
    fn bridge_header_type() {
        let env = Env::new();
        let bridge = Bridge::new();
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
        let bridge = Bridge::new();
        let vals: Vec<u8> = vec![24, 25, 26];
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
}
