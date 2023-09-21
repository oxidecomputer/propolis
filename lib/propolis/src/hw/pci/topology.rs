// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A PCI topology containing one or more PCI buses.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{Error as IoError, ErrorKind};
use std::sync::{Arc, Mutex};

use crate::common::RWOp;
use crate::hw::ids;
use crate::inventory::{Inventory, RegistrationError};
use crate::vmm::Machine;

use super::bridge::Bridge;
use super::{Bdf, Bus, BusLocation, Endpoint, LintrCfg};

use thiserror::Error;

/// A logical identifier for a bus in the topology. A bus's logical identifer
/// is stable irrespective of the way the topology's bridges are configured.
#[derive(Clone, Copy, Debug, Ord, PartialOrd, Eq, PartialEq)]
pub struct LogicalBusId(pub u8);

/// A "routing" identifier for a bus in the topology. The topology considers
/// bridge configurations when deciding what bus will receive messages directed
/// using this kind of ID.
#[derive(Clone, Copy, Ord, PartialOrd, Eq, PartialEq)]
pub struct RoutedBusId(pub u8);

#[derive(Clone, Copy)]
struct BusIndex(usize);

/// Errors returned when manipulating PCI topology.
#[derive(Debug, Error)]
pub enum PciTopologyError {
    #[error("The logical bus with ID {0:?} was not found")]
    LogicalBusNotFound(LogicalBusId),

    #[error("Downstream logical bus ID {0:?} was already registered")]
    LogicalBusAlreadyExists(LogicalBusId),

    #[error("A PCI device was already attached at {0:?}")]
    DeviceAlreadyAttached(Bdf),

    #[error("Failed to register a bridge with error {0:?}")]
    BridgeRegistrationError(RegistrationError),
}

impl From<PciTopologyError> for IoError {
    fn from(e: PciTopologyError) -> IoError {
        use PciTopologyError::*;
        match e {
            LogicalBusNotFound(b) => IoError::new(
                ErrorKind::NotFound,
                format!("Logical bus {} not found", b.0),
            ),
            LogicalBusAlreadyExists(b) => IoError::new(
                ErrorKind::AlreadyExists,
                format!("Logical bus {} already exists", b.0),
            ),
            DeviceAlreadyAttached(bdf) => IoError::new(
                ErrorKind::AlreadyExists,
                format!("Device at {} already attached", bdf),
            ),
            BridgeRegistrationError(e) => IoError::from(e),
        }
    }
}

/// A PCI topology manager.
pub struct Topology {
    buses: Vec<Bus>,
    logical_buses: BTreeMap<LogicalBusId, BusIndex>,
    inner: Mutex<Inner>,
}

impl Topology {
    /// Attaches a device to a logical bus in this topology.
    ///
    /// # Errors
    ///
    /// Fails if the logical bus is not present in the topology.
    pub fn pci_attach(
        &self,
        bus: LogicalBusId,
        location: BusLocation,
        dev: Arc<dyn Endpoint>,
        lintr_cfg: Option<LintrCfg>,
    ) -> Result<(), PciTopologyError> {
        if let Some(bus_index) = self.logical_buses.get(&bus) {
            let bus = &self.buses[bus_index.0];
            bus.attach(location, dev, lintr_cfg);
            Ok(())
        } else {
            Err(PciTopologyError::LogicalBusNotFound(bus))
        }
    }

    /// Issues a configuration space I/O to a device at the supplied location.
    pub fn pci_cfg_rw(
        &self,
        bus: RoutedBusId,
        location: BusLocation,
        rwo: RWOp,
    ) -> Option<()> {
        let guard = self.inner.lock().unwrap();
        let device = match guard.routed_buses.get(&bus) {
            Some(bus_index) => {
                let bus = &self.buses[bus_index.0];
                bus.device_at(location)
            }
            None => None,
        };

        // Don't call into the device with the lock held to avoid recursive
        // acquisition (the device may be a bridge, and this operation may need
        // to reconfigure part of the topology).
        drop(guard);
        if let Some(device) = device {
            device.cfg_rw(rwo);
            Some(())
        } else {
            None
        }
    }

    /// Configures the topology so that routed traffic to the supplied routed
    /// bus ID will be directed to the supplied logical bus (if `logical_id` is
    /// Some) or to no logical bus (if it is None).
    pub(super) fn set_bus_route(
        &self,
        routed_id: RoutedBusId,
        logical_id: Option<LogicalBusId>,
    ) {
        // This is only used by PCI topology elements like bridges that know
        // their own logical bus numbers, so absent a code bug the index
        // corresponding to this logical bus should always be in the map.
        if let Some(logical_id) = logical_id {
            let bus_index =
                self.logical_buses.get(&logical_id).unwrap_or_else(|| {
                    panic!(
                        "Failed to find logical bus {} while routing bus {}",
                        logical_id.0, routed_id.0
                    )
                });
            let mut guard = self.inner.lock().unwrap();
            let _old = guard.routed_buses.insert(routed_id, *bus_index);
            assert!(_old.is_none());
        } else {
            let mut guard = self.inner.lock().unwrap();
            let _old = guard.routed_buses.remove(&routed_id);
            assert!(_old.is_some());
        }
    }
}

struct Inner {
    routed_buses: BTreeMap<RoutedBusId, BusIndex>,
}
impl Inner {
    fn new() -> Self {
        Self { routed_buses: BTreeMap::new() }
    }
}

/// An abstract description of a PCI bridge that should be added to a topology.
#[derive(Debug, Clone, Copy)]
pub struct BridgeDescription {
    downstream_bus_id: LogicalBusId,
    attachment_addr: Bdf,
    vendor_id: u16,
    device_id: u16,
}

impl BridgeDescription {
    /// Creates a new PCI bridge description using the Oxide PCI-PCI bridge
    /// vendor and device IDs.
    ///
    /// # Arguments
    ///
    /// - `downstream_bus_id`: The logical bus ID to associate with the bridge's
    ///   downstream bus.
    /// - `attachment_addr`: The bus/device/function at which to attach the
    ///   bridge, where the bus is a logical bus number. A bridge may attach to
    ///   the downstream bus of another bridge.
    pub fn new(downstream_bus_id: LogicalBusId, attachment_addr: Bdf) -> Self {
        Self::with_pci_ids(
            downstream_bus_id,
            attachment_addr,
            ids::pci::VENDOR_OXIDE,
            ids::pci::PROPOLIS_BRIDGE_DEV_ID,
        )
    }

    /// Creates a new PCI bridge description with an explicitly supplied vendor
    /// and device ID. See the documentation for [`new`](Self::new).
    pub fn with_pci_ids(
        downstream_bus_id: LogicalBusId,
        attachment_addr: Bdf,
        vendor_id: u16,
        device_id: u16,
    ) -> Self {
        Self { downstream_bus_id, attachment_addr, vendor_id, device_id }
    }
}

/// A builder used to construct a PCI topology incrementally.
pub struct Builder {
    bridges: Vec<BridgeDescription>,
    downstream_buses: BTreeSet<LogicalBusId>,
    attachment_addrs: BTreeSet<Bdf>,
}

impl Builder {
    /// Creates a new topology builder. Buses created by this builder will
    /// associate themselves with the supplied port I/O and MMIO buses.
    pub fn new() -> Self {
        let mut this = Self {
            bridges: Vec::new(),
            downstream_buses: BTreeSet::new(),
            attachment_addrs: BTreeSet::new(),
        };
        this.downstream_buses.insert(LogicalBusId(0));
        this
    }

    /// Asks the builder to create a new PCI-PCI bridge.
    ///
    /// # Errors
    ///
    /// Fails if a bridge was already registered with the same logical bus or
    /// the same attachment address as the bridge being registered.
    pub fn add_bridge(
        &mut self,
        desc: BridgeDescription,
    ) -> Result<(), PciTopologyError> {
        if self.downstream_buses.contains(&desc.downstream_bus_id) {
            Err(PciTopologyError::LogicalBusAlreadyExists(
                desc.downstream_bus_id,
            ))
        } else if self.attachment_addrs.contains(&desc.attachment_addr) {
            Err(PciTopologyError::DeviceAlreadyAttached(desc.attachment_addr))
        } else {
            self.downstream_buses.insert(desc.downstream_bus_id);
            self.attachment_addrs.insert(desc.attachment_addr);
            self.bridges.push(desc);
            Ok(())
        }
    }

    /// Constructs a completed topology with the requested buses and bridges.
    ///
    /// # Errors
    ///
    /// Fails if a bridge had an invalid attachment address (i.e. one whose
    /// logical bus number is invalid).
    pub fn finish(
        self,
        inventory: &Inventory,
        machine: &Machine,
    ) -> Result<Arc<Topology>, PciTopologyError> {
        let mut buses = Vec::new();
        let mut logical_buses = BTreeMap::new();
        let mut inner = Inner::new();

        let pio_bus = &machine.bus_pio;
        let mmio_bus = &machine.bus_mmio;

        // Bus 0 is always present and always routes to itself.
        buses.push(Bus::new(
            pio_bus,
            mmio_bus,
            machine.acc_mem.child(Some("PCI bus 0".to_string())),
            machine.acc_msi.child(Some("PCI bus 0".to_string())),
        ));
        logical_buses.insert(LogicalBusId(0), BusIndex(0));
        inner.routed_buses.insert(RoutedBusId(0), BusIndex(0));

        for bridge in &self.bridges {
            let idx = buses.len();
            logical_buses.insert(
                LogicalBusId(bridge.downstream_bus_id.0),
                BusIndex(idx),
            );
            // TODO: wire up accessors to mirror actual bus topology
            buses.push(Bus::new(
                &pio_bus,
                &mmio_bus,
                machine.acc_mem.child(Some(format!("PCI bus {idx}"))),
                machine.acc_msi.child(Some(format!("PCI bus {idx}"))),
            ));
        }

        let topology = Arc::new(Topology {
            buses,
            logical_buses,
            inner: Mutex::new(inner),
        });

        for bridge in &self.bridges {
            let new_bridge = Bridge::new(
                bridge.vendor_id,
                bridge.device_id,
                &topology,
                bridge.downstream_bus_id,
            );
            if let Err(e) =
                inventory.register_instance(&new_bridge, bridge.attachment_addr)
            {
                return Err(PciTopologyError::BridgeRegistrationError(e));
            }
            topology.pci_attach(
                LogicalBusId(bridge.attachment_addr.bus.get()),
                bridge.attachment_addr.location,
                new_bridge,
                None,
            )?;
        }

        Ok(topology)
    }
}

#[cfg(test)]
mod test {
    use crate::common::ReadOp;
    use crate::instance::Instance;

    use super::*;

    #[test]
    fn build_without_bridges() {
        let inst = Instance::new_test().unwrap();
        let builder = Builder::new();

        let guard = inst.lock();
        assert!(builder.finish(guard.inventory(), guard.machine()).is_ok());
    }

    #[test]
    fn build_with_bridges() {
        let inst = Instance::new_test().unwrap();
        let mut builder = Builder::new();

        assert!(builder
            .add_bridge(BridgeDescription::new(
                LogicalBusId(1),
                Bdf::new(0, 1, 0).unwrap(),
            ))
            .is_ok());
        assert!(builder
            .add_bridge(BridgeDescription::new(
                LogicalBusId(4),
                Bdf::new(0, 4, 0).unwrap(),
            ))
            .is_ok());

        let guard = inst.lock();
        assert!(builder.finish(guard.inventory(), guard.machine()).is_ok());
    }

    #[test]
    fn builder_bus_zero_reserved() {
        let mut builder = Builder::new();
        assert!(builder
            .add_bridge(BridgeDescription::new(
                LogicalBusId(0),
                Bdf::new(0, 3, 0).unwrap()
            ))
            .is_err());
    }

    #[test]
    fn builder_conflicts() {
        let mut builder = Builder::new();
        assert!(builder
            .add_bridge(BridgeDescription::new(
                LogicalBusId(7),
                Bdf::new(0, 7, 0).unwrap()
            ))
            .is_ok());
        assert!(builder
            .add_bridge(BridgeDescription::new(
                LogicalBusId(7),
                Bdf::new(0, 4, 0).unwrap()
            ))
            .is_err());
        assert!(builder
            .add_bridge(BridgeDescription::new(
                LogicalBusId(4),
                Bdf::new(0, 7, 0).unwrap()
            ))
            .is_err());
    }

    #[test]
    fn cfg_read() {
        let inst = Instance::new_test().unwrap();
        let mut builder = Builder::new();
        assert!(builder
            .add_bridge(BridgeDescription::new(
                LogicalBusId(1),
                Bdf::new(0, 1, 0).unwrap()
            ))
            .is_ok());

        let guard = inst.lock();
        let topology =
            builder.finish(guard.inventory(), guard.machine()).unwrap();
        let mut buf = [0u8; 1];
        let mut ro = ReadOp::from_buf(0, &mut buf);
        assert!(topology
            .pci_cfg_rw(
                RoutedBusId(0),
                BusLocation::new(1, 0).unwrap(),
                RWOp::Read(&mut ro),
            )
            .is_some());
        assert!(topology
            .pci_cfg_rw(
                RoutedBusId(1),
                BusLocation::new(1, 0).unwrap(),
                RWOp::Read(&mut ro),
            )
            .is_none());
    }

    fn inventory_count(inv: &Inventory) -> usize {
        let mut count = 0;
        inv.for_each_node(
            crate::inventory::Order::Pre,
            |_, _| -> Result<(), ()> {
                count += 1;
                Ok(())
            },
        )
        .unwrap();
        count
    }

    #[test]
    fn registered_bridges() {
        let inst = Instance::new_test().unwrap();

        let guard = inst.lock();
        let before = inventory_count(guard.inventory());

        let mut builder = Builder::new();
        assert!(builder
            .add_bridge(BridgeDescription::new(
                LogicalBusId(1),
                Bdf::new(0, 1, 0).unwrap()
            ))
            .is_ok());
        let _topology =
            builder.finish(guard.inventory(), guard.machine()).unwrap();
        let after = inventory_count(guard.inventory());
        assert!(after > before);
    }
}
