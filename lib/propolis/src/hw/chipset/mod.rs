// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::sync::Arc;

use crate::hw::pci::{Bdf, Endpoint};
use crate::intr_pins::IntrPin;

pub mod i440fx;
mod piix3_ide;

pub trait Chipset {
    fn pci_attach(&self, bdf: Bdf, dev: Arc<dyn Endpoint>);
    fn irq_pin(&self, irq: u8) -> Option<Box<dyn IntrPin>>;
    fn power_pin(&self) -> Arc<dyn IntrPin>;
    fn reset_pin(&self) -> Arc<dyn IntrPin>;
}
