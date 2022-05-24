//! A PCI topology containing one or more PCI buses.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

use crate::common::RWOp;
use crate::dispatch::DispCtx;
use crate::mmio::MmioBus;
use crate::pio::PioBus;

use super::bridge::Bridge;
use super::{bits, Bdf, Bus, BusLocation, Endpoint, LintrCfg};

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
        ctx: &DispCtx,
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
            device.cfg_rw(rwo, ctx);
            Some(())
        } else {
            None
        }
    }

    /// Configures the topology so that routed traffic to the supplied routed
    /// bus ID will be directed to the bus with the supplied logical ID.
    pub(super) fn set_bus_route(
        &self,
        routed_id: RoutedBusId,
        logical_id: Option<LogicalBusId>,
    ) {
        // This is only used by PCI topology elements like bridges that know
        // their own logical bus numbers, so absent a code bug the index
        // corresponding to this logical bus should always be in the map.
        if let Some(logical_id) = logical_id {
            let bus_index = self.logical_buses.get(&logical_id).unwrap();
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

#[derive(Default)]
struct Inner {
    routed_buses: BTreeMap<RoutedBusId, BusIndex>,
}

/// An abstract description of a PCI bridge that should be added to a topology.
#[derive(Debug, Clone, Copy)]
pub struct BridgeDescription {
    downstream_bus_id: LogicalBusId,
    attachment_addr: Bdf,
}

impl BridgeDescription {
    /// Creates a new PCI bridge description.
    ///
    /// # Arguments
    ///
    /// - `downstream_bus_id`: The logical bus ID to associate with the bridge's
    ///   downstream bus.
    /// - `attachment_addr`: The bus/device/function at which to attach the
    ///   bridge, where the bus is a logical bus number. A bridge may attach to
    ///   the downstream bus of another bridge.
    pub fn new(downstream_bus_id: LogicalBusId, attachment_addr: Bdf) -> Self {
        Self { downstream_bus_id, attachment_addr }
    }
}

/// A builder used to construct a PCI topology incrementally.
pub struct Builder<'a> {
    pio_bus: &'a Arc<PioBus>,
    mmio_bus: &'a Arc<MmioBus>,

    bridges: Vec<BridgeDescription>,
    downstream_buses: BTreeSet<LogicalBusId>,
    attachment_addrs: BTreeSet<Bdf>,
}

impl<'a> Builder<'a> {
    /// Creates a new topology builder. Buses created by this builder will
    /// associate themselves with the supplied port I/O and MMIO buses.
    pub fn new(pio_bus: &'a Arc<PioBus>, mmio_bus: &'a Arc<MmioBus>) -> Self {
        let mut this = Self {
            pio_bus,
            mmio_bus,
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
    pub fn finish(self) -> Result<Arc<Topology>, PciTopologyError> {
        let mut buses = Vec::new();
        let mut logical_buses = BTreeMap::new();
        let mut inner = Inner::default();

        // Bus 0 is always present and always routes to itself.
        buses.push(Bus::new(&self.pio_bus, &self.mmio_bus));
        logical_buses.insert(LogicalBusId(0), BusIndex(0));
        inner.routed_buses.insert(RoutedBusId(0), BusIndex(0));

        for bridge in &self.bridges {
            logical_buses.insert(
                LogicalBusId(bridge.downstream_bus_id.0),
                BusIndex(buses.len()),
            );
            buses.push(Bus::new(&self.pio_bus, &self.mmio_bus));
        }

        let topology = Arc::new(Topology {
            buses,
            logical_buses,
            inner: Mutex::new(inner),
        });

        for bridge in &self.bridges {
            let new_bridge = Bridge::new(
                bits::BRIDGE_VENDOR_ID,
                bits::BRIDGE_DEVICE_ID,
                topology.clone(),
                bridge.downstream_bus_id,
            );
            topology
                .pci_attach(
                    LogicalBusId(bridge.attachment_addr.bus.get()),
                    bridge.attachment_addr.location,
                    new_bridge,
                    None,
                )
                .unwrap();
        }

        Ok(topology)
    }
}
