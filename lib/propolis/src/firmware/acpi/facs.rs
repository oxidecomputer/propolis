// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Generates a FACS ACPI table for an instance.
//!
//! The [`Facs`] struct implements the `Aml` trait of the `acpi_tables` crate
//! and can write the AML bytecode to any AmlSink, like a `Vec<u8>`.

use super::AcpiVariant;
use acpi_tables::{facs, Aml, AmlSink};

/// Configuration for generating a FACS table.
pub struct FacsConfig {
    /// The ACPI table variant to use.
    pub acpi_variant: AcpiVariant,
}

/// The FACS table stores information about the firmware.
///
/// ACPI rev. 6.6 section 5.2.10 "Firmware ACPI Control Structure (FACS)"
pub struct Facs {
    config: FacsConfig,
}

impl Facs {
    pub fn new(config: FacsConfig) -> Self {
        Self { config }
    }
}

impl Aml for Facs {
    fn to_aml_bytes(&self, sink: &mut dyn AmlSink) {
        match self.config.acpi_variant {
            AcpiVariant::V0 => {
                // The acpi_tables crate generates version 1 of the FACS table
                // while the original static EDK2 table was version 0. The only
                // difference is the addition of the X_Firmware_Waking_Vector
                // field, which is not used by Propolis.
                facs::FACS::new().to_aml_bytes(sink);
            }
        }
    }
}
