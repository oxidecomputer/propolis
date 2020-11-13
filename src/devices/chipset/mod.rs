use std::sync::Arc;

use crate::dispatch::DispCtx;
use crate::pci::{PciBDF, PciEndpoint};

pub mod i440fx;

pub trait Chipset {
    fn pci_attach(&self, bdf: PciBDF, dev: Arc<dyn PciEndpoint>);
    fn pci_finalize(&self, ctx: &DispCtx);
}
