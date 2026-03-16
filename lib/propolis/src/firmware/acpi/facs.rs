// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Generates a FACS ACPI table for an instance.
//!
//! The [`Facs`] struct implements the `Aml` trait of the `acpi_tables` crate
//! and can write the AML bytecode to any AmlSink, like a `Vec<u8>`.

use acpi_tables::{facs, Aml, AmlSink};

/// The FACS table stores information about the firmware.
///
/// https://uefi.org/htmlspecs/ACPI_Spec_6_4_html/05_ACPI_Software_Programming_Model/ACPI_Software_Programming_Model.html#firmware-acpi-control-structure-facs
pub struct Facs {}

impl Facs {
    pub fn new() -> Self {
        Self {}
    }
}

// XXX(acpi): The acpi_tables crate generates version 1 of the FACS table while
//            the original static EDK2 table was version 0. The only difference
//            is the addition of the X_Firmware_Waking_Vector field, which is
//            not used by Propolis.
impl Aml for Facs {
    fn to_aml_bytes(&self, sink: &mut dyn AmlSink) {
        facs::FACS::new().to_aml_bytes(sink);
    }
}
