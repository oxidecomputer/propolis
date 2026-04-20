// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Generates an XSDT ACPI table for an instance.
//!
//! The [`Xsdt`] struct implements the `Aml` trait of the `acpi_tables` crate
//! and can write the AML bytecode to any AmlSink, like a `Vec<u8>`.

use super::{OEM_ID, OEM_REVISION, OEM_TABLE_ID};
use acpi_tables::{xsdt, Aml, AmlSink};

// Byte offset and length of fields that need to be referenced during table
// generation.
pub const XSDT_HEADER_LEN: usize = 36;

/// The XSDT table provides the addresses of additional tables.
///
/// <https://uefi.org/htmlspecs/ACPI_Spec_6_4_html/05_ACPI_Software_Programming_Model/ACPI_Software_Programming_Model.html#extended-system-description-table-xsdt>
pub struct Xsdt {
    entries: Vec<u64>,
}

impl Xsdt {
    pub fn new(entries: Vec<u64>) -> Self {
        Self { entries }
    }
}

impl Aml for Xsdt {
    fn to_aml_bytes(&self, sink: &mut dyn AmlSink) {
        let mut table = xsdt::XSDT::new(*OEM_ID, *OEM_TABLE_ID, OEM_REVISION);
        self.entries.iter().for_each(|e| table.add_entry(*e));
        table.to_aml_bytes(sink);
    }
}
