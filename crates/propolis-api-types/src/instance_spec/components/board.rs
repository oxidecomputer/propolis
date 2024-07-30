// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! VM mainboard components. Every VM has a board, even if it has no other
//! peripherals.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::BootSettings;

/// An Intel 440FX-compatible chipset.
#[derive(
    Clone, Copy, Deserialize, Serialize, Debug, PartialEq, Eq, JsonSchema,
)]
#[serde(deny_unknown_fields)]
pub struct I440Fx {
    /// Specifies whether the chipset should allow PCI configuration space
    /// to be accessed through the PCIe extended configuration mechanism.
    pub enable_pcie: bool,
}

/// A kind of virtual chipset.
#[derive(
    Clone, Copy, Deserialize, Serialize, Debug, PartialEq, Eq, JsonSchema,
)]
#[serde(
    deny_unknown_fields,
    rename_all = "snake_case",
    tag = "type",
    content = "value"
)]
pub enum Chipset {
    /// An Intel 440FX-compatible chipset.
    I440Fx(I440Fx),
}

/// A VM's mainboard.
#[derive(Clone, Deserialize, Serialize, Debug, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Board {
    /// The number of virtual logical processors attached to this VM.
    pub cpus: u8,

    /// The amount of guest RAM attached to this VM.
    pub memory_mb: u64,

    /// The chipset to expose to guest software.
    pub chipset: Chipset,

    /// The boot device order to supply to the guest.
    pub boot_settings: BootSettings,
    // TODO: Guest platform and CPU feature identification.
    // TODO: NUMA topology.
}

impl Default for Board {
    fn default() -> Self {
        Self {
            cpus: 0,
            memory_mb: 0,
            chipset: Chipset::I440Fx(I440Fx { enable_pcie: false }),
            boot_settings: BootSettings { order: vec![] },
        }
    }
}
