use std::sync::Arc;

use crate::dispatch::DispCtx;
use crate::hw::pci::{Bdf, Endpoint};
use crate::intr_pins::LegacyPin;

pub mod i440fx;

pub trait Chipset {
    fn pci_attach(&self, bdf: Bdf, dev: Arc<dyn Endpoint>);
    fn pci_finalize(&self, ctx: &DispCtx);
    fn irq_pin(&self, irq: u8) -> Option<LegacyPin>;
}
