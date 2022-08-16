use std::sync::Arc;

use crate::hw::pci::{Bdf, Endpoint};
use crate::intr_pins::IntrPin;

pub mod i440fx;

pub trait Chipset {
    fn pci_attach(&self, bdf: Bdf, dev: Arc<dyn Endpoint>);
    fn irq_pin(&self, irq: u8) -> Option<Box<dyn IntrPin>>;
    fn power_pin(&self) -> Arc<dyn IntrPin>;
    fn reset_pin(&self) -> Arc<dyn IntrPin>;
}
