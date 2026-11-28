// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::sync::Arc;

use thiserror::Error;

use crate::hw::pci::{topology::PciTopologyError, Bdf, Endpoint, LintrCfg};
use crate::intr_pins::IntrPin;

pub mod i440fx;

#[derive(Debug, Error)]
pub enum ChipsetError {
    #[error(transparent)]
    PciTopologyError(#[from] PciTopologyError),
}

pub trait Chipset: Send + Sync {
    fn pci_attach(
        &self,
        bdf: Bdf,
        dev: Arc<dyn Endpoint>,
        lintr_cfg: Option<LintrCfg>,
    );
    fn pci_detach(
        &self,
        bdf: Bdf,
    ) -> Result<Option<Arc<dyn Endpoint>>, ChipsetError>;
    fn power_pin(&self) -> Arc<dyn IntrPin>;
    fn reset_pin(&self) -> Arc<dyn IntrPin>;
}
