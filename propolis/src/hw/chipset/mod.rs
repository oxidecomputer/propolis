use std::sync::Arc;

use crate::hw::pci::{router::Router, Bus, BusLocation, Endpoint};
use crate::intr_pins::LegacyPin;

pub mod i440fx;

pub trait Chipset {
    fn pci_root_bus(&self) -> &Bus;
    fn pci_router(&self) -> &Arc<Router>;
    fn pci_attach(
        &self,
        bus: &Bus,
        location: BusLocation,
        dev: Arc<dyn Endpoint>,
    );
    fn irq_pin(&self, irq: u8) -> Option<LegacyPin>;
}
