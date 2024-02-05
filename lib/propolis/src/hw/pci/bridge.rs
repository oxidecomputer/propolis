// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Support for PCI bridges.

use std::num::NonZeroU8;
use std::sync::{Arc, Mutex, Weak};

use super::bus::Attachment;
use super::cfgspace::{CfgBuilder, CfgReg};
use super::topology::{LogicalBusId, RoutedBusId, Topology};
use super::{bits::*, Endpoint, Ident};
use super::{BarN, BusNum, StdCfgReg};
use crate::common::{Lifecycle, RWOp, ReadOp, WriteOp};
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
    ident: Ident,

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
    pub fn new(
        vendor: u16,
        device: u16,
        topology: &Arc<Topology>,
        downstream_bus_id: LogicalBusId,
    ) -> Arc<Self> {
        let cfg_builder = CfgBuilder::new();
        Arc::new(Self {
            ident: Ident {
                vendor_id: vendor,
                device_id: device,
                sub_vendor_id: vendor,
                sub_device_id: device,
                class: BRIDGE_PROG_CLASS,
                subclass: BRIDGE_PROG_SUBCLASS,
                prog_if: BRIDGE_PROG_IF,
                ..Default::default()
            },
            cfg_map: cfg_builder.finish().0,
            inner: Mutex::new(Inner::new(topology, downstream_bus_id)),
        })
    }

    fn cfg_header_rw(&self, mut rwo: RWOp) {
        CFG_HEADER_MAP.process(&mut rwo, |id, rwo| match rwo {
            RWOp::Read(ro) => {
                self.cfg_std_read(id, ro);
            }
            RWOp::Write(wo) => {
                self.cfg_std_write(id, wo);
            }
        })
    }

    fn cfg_std_read(&self, id: &BridgeReg, ro: &mut ReadOp) {
        match id {
            BridgeReg::Common(id) => match id {
                StdCfgReg::VendorId => ro.write_u16(self.ident.vendor_id),
                StdCfgReg::DeviceId => ro.write_u16(self.ident.device_id),
                StdCfgReg::Class => ro.write_u8(self.ident.class),
                StdCfgReg::Subclass => ro.write_u8(self.ident.subclass),
                StdCfgReg::SubVendorId => {
                    ro.write_u16(self.ident.sub_vendor_id)
                }
                StdCfgReg::SubDeviceId => {
                    ro.write_u16(self.ident.sub_device_id)
                }
                StdCfgReg::ProgIf => ro.write_u8(self.ident.prog_if),
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

    fn cfg_std_write(&self, id: &BridgeReg, wo: &mut WriteOp) {
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
                guard.primary_bus = BusNum::new(wo.read_u8())
            }
            BridgeReg::SecondaryBus => {
                let mut guard = self.inner.lock().unwrap();
                guard.set_secondary_bus(BusNum::new(wo.read_u8()))
            }
            BridgeReg::SubordinateBus => {
                let mut guard = self.inner.lock().unwrap();
                guard.subordinate_bus = BusNum::new(wo.read_u8())
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

    fn cfg_rw(&self, mut rwo: RWOp) {
        self.cfg_map.process(&mut rwo, |id, rwo| match id {
            CfgReg::Std => {
                self.cfg_header_rw(rwo);
            }
            _ => {
                panic!(
                    "Unexpected read of bridge config space with ID {:?}",
                    id
                )
            }
        });
    }

    fn bar_rw(&self, _bar: BarN, _rwo: RWOp) {
        // Bridges don't consume any additional I/O or memory space that would
        // be described in a BAR (and indeed their BARs are read-only), so this
        // routine should never be reached.
        panic!("unexpected BAR-defined region I/O in PCI bridge");
    }
}

impl Lifecycle for Bridge {
    fn type_name(&self) -> &'static str {
        "pci-bridge"
    }
    fn reset(&self) {
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

    // This reference must be weak to avoid a topology -> bus -> attached
    // bridge -> topology reference cycle.
    topology: Weak<Topology>,
    downstream_bus_id: LogicalBusId,

    reg_command: RegCmd,
    primary_bus: BusNum,
    secondary_bus: BusNum,
    subordinate_bus: BusNum,
    memory_base: u16,
    memory_limit: u16,
}

impl Inner {
    fn new(topology: &Arc<Topology>, downstream_bus_id: LogicalBusId) -> Self {
        Self {
            attachment: None,
            topology: Arc::downgrade(topology),
            downstream_bus_id,
            reg_command: RegCmd::empty(),
            primary_bus: BusNum::new(0),
            secondary_bus: BusNum::new(0),
            subordinate_bus: BusNum::new(0),
            memory_base: 0,
            memory_limit: 0,
        }
    }

    fn set_secondary_bus(&mut self, n: BusNum) {
        let topology = self.topology.upgrade();
        if let Some(bus) = NonZeroU8::new(self.secondary_bus.get()) {
            if let Some(topology) = &topology {
                topology.set_bus_route(RoutedBusId(bus.get()), None);
            }
        }
        self.secondary_bus = n;
        if let Some(bus) = NonZeroU8::new(self.secondary_bus.get()) {
            if let Some(topology) = &topology {
                topology.set_bus_route(
                    RoutedBusId(bus.get()),
                    Some(self.downstream_bus_id),
                );
            }
        }
    }

    fn reset(&mut self) {
        self.primary_bus = BusNum::new(0);
        self.set_secondary_bus(BusNum::new(0));
        self.subordinate_bus = BusNum::new(0);
        self.memory_base = 0;
        self.memory_limit = 0;
    }
}

#[cfg(test)]
mod test {
    use crate::hw::ids;
    use crate::hw::pci::topology::{
        BridgeDescription, Builder, FinishedTopology, LogicalBusId,
    };
    use crate::hw::pci::{Bdf, Endpoint};
    use crate::vmm::Machine;

    use super::*;

    const OFFSET_VENDOR_ID: usize = 0x00;
    const OFFSET_DEVICE_ID: usize = 0x02;
    const OFFSET_HEADER_TYPE: usize = 0x0E;
    const OFFSET_SECONDARY_BUS: usize = 0x19;

    struct Env {
        _machine: Machine,
        topology: Arc<Topology>,
    }

    impl Env {
        fn new(bridges: Option<Vec<BridgeDescription>>) -> Self {
            let mut builder = Builder::new();
            if let Some(bridges) = bridges {
                for bridge in bridges {
                    builder.add_bridge(bridge).unwrap();
                }
            }

            let machine = Machine::new_test().unwrap();
            let FinishedTopology { topology, bridges: _bridges } =
                builder.finish(&machine).unwrap();
            Self { _machine: machine, topology }
        }

        fn make_bridge(&self) -> Arc<Bridge> {
            Bridge::new(
                ids::pci::VENDOR_OXIDE,
                ids::pci::PROPOLIS_BRIDGE_DEV_ID,
                &self.topology,
                LogicalBusId(0xFF),
            )
        }

        fn read_header_byte(&self, target: Bdf, offset: usize) -> u8 {
            let mut buf = [0u8; 1];
            let mut ro = ReadOp::from_buf(offset, &mut buf);
            self.topology.pci_cfg_rw(
                RoutedBusId(target.bus.get()),
                target.location,
                RWOp::Read(&mut ro),
            );
            buf[0]
        }

        fn read_header_type(&self, target: Bdf) -> u8 {
            self.read_header_byte(target, OFFSET_HEADER_TYPE)
        }

        fn read_secondary_bus(&self, target: Bdf) -> u8 {
            self.read_header_byte(target, OFFSET_SECONDARY_BUS)
        }

        fn write_header_byte(&self, target: Bdf, offset: usize, val: u8) {
            let mut buf = [val; 1];
            let mut wo = WriteOp::from_buf(offset, &mut buf);
            self.topology.pci_cfg_rw(
                RoutedBusId(target.bus.get()),
                target.location,
                RWOp::Write(&mut wo),
            );
        }

        fn write_secondary_bus(&self, target: Bdf, val: u8) {
            self.write_header_byte(target, OFFSET_SECONDARY_BUS, val);
        }
    }

    #[test]
    fn bridge_properties() {
        let env = Env::new(None);
        let bridge = env.make_bridge();
        let mut buf = [0xffu8; 1];
        let mut ro = ReadOp::from_buf(OFFSET_HEADER_TYPE, &mut buf);
        Endpoint::cfg_rw(bridge.as_ref(), RWOp::Read(&mut ro));
        assert_eq!(buf[0], HEADER_TYPE_BRIDGE);

        let mut buf = [0xffu8; 2];
        let mut ro = ReadOp::from_buf(OFFSET_VENDOR_ID, &mut buf);
        Endpoint::cfg_rw(bridge.as_ref(), RWOp::Read(&mut ro));
        assert_eq!(u16::from_le_bytes(buf), ids::pci::VENDOR_OXIDE);

        let mut buf = [0xffu8; 2];
        let mut ro = ReadOp::from_buf(OFFSET_DEVICE_ID, &mut buf);
        Endpoint::cfg_rw(bridge.as_ref(), RWOp::Read(&mut ro));
        assert_eq!(u16::from_le_bytes(buf), ids::pci::PROPOLIS_BRIDGE_DEV_ID);
    }

    #[test]
    fn bridge_routing() {
        let env = Env::new(Some(vec![
            BridgeDescription::new(LogicalBusId(1), Bdf::new(0, 1, 0).unwrap()),
            BridgeDescription::new(LogicalBusId(2), Bdf::new(0, 2, 0).unwrap()),
            BridgeDescription::new(LogicalBusId(3), Bdf::new(1, 1, 0).unwrap()),
            BridgeDescription::new(LogicalBusId(4), Bdf::new(2, 1, 0).unwrap()),
        ]));

        // Set the first test bridge's downstream bus to 81, then verify that
        // 81.1.0 is a valid bridge device.
        env.write_secondary_bus(Bdf::new(0, 1, 0).unwrap(), 81);
        assert_eq!(
            env.read_header_type(Bdf::new(81, 1, 0).unwrap()),
            HEADER_TYPE_BRIDGE
        );

        // Write bus 83 to the newly-connected downstream bridge's secondary
        // bus number to distinguish it from the other, uninitialized bridge.
        env.write_secondary_bus(Bdf::new(81, 1, 0).unwrap(), 83);

        // Clear the test bridge's secondary bus register and verify the routing
        // is removed.
        env.write_secondary_bus(Bdf::new(0, 1, 0).unwrap(), 0);
        assert_eq!(env.read_secondary_bus(Bdf::new(81, 1, 0).unwrap()), 0);

        // Set the second parent bridge's downstream bus to 81. The downstream
        // bridge's secondary bus should not be set.
        env.write_secondary_bus(Bdf::new(0, 2, 0).unwrap(), 81);
        assert_eq!(
            env.read_header_type(Bdf::new(81, 1, 0).unwrap()),
            HEADER_TYPE_BRIDGE
        );
        assert_eq!(env.read_secondary_bus(Bdf::new(81, 1, 0).unwrap()), 0);

        // Route the first parent bridge to downstream bus 82 and verify the
        // child bridge with bus 83 is still there.
        env.write_secondary_bus(Bdf::new(0, 1, 0).unwrap(), 82);
        assert_eq!(env.read_secondary_bus(Bdf::new(82, 1, 0).unwrap()), 83);

        // Clear the second bridge's routing and verify that the first bridge's
        // routing is left alone.
        env.write_secondary_bus(Bdf::new(0, 2, 0).unwrap(), 0);
        assert_eq!(env.read_secondary_bus(Bdf::new(82, 1, 0).unwrap()), 83);

        // Clear the first bridge's routing and verify that its entry is also
        // safely removed.
        env.write_secondary_bus(Bdf::new(0, 1, 0).unwrap(), 0);
        assert_eq!(env.read_secondary_bus(Bdf::new(82, 1, 0).unwrap()), 0);
    }
}
