// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Generates a RSDP ACPI table for an instance.
//!
//! The [`Rsdp`] struct implements the `Aml` trait of the `acpi_tables` crate
//! and can write the AML bytecode to any AmlSink, like a `Vec<u8>`.

use super::OEM_ID;
use acpi_tables::{rsdp, Aml, AmlSink};

// Byte offset and length of fields that need to be referenced during table
// generation.
pub const RSDP_XSDT_ADDR_OFFSET: usize = 24;
pub const RSDP_XSDT_ADDR_LEN: usize = 8;

// The RSDP table has two checksums fields.
//
// - RSDP_V1_CHECKSUM_* points to the original checksum field defined in the
//   ACPI 1.0 specification.
//
// - RSDP_EXTENDED_CHECKSUM_* points to the new checksum field that includes
//   the entire table.
pub const RSDP_V1_CHECKSUM_OFFSET: usize = 8;
pub const RSDP_V1_TABLE_LEN: usize = 20;

pub const RSDP_EXTENDED_CHECKSUM_OFFSET: usize = 32;
pub const RSDP_EXTENDED_TABLE_LEN: usize = 36;

/// The RSDP table is the root table the operating system loads first to
/// discover the other tables.
///
/// <https://uefi.org/htmlspecs/ACPI_Spec_6_4_html/05_ACPI_Software_Programming_Model/ACPI_Software_Programming_Model.html#root-system-description-pointer-rsdp>
pub struct Rsdp {
    xsdt_addr: u64,
}

impl Rsdp {
    pub fn new(xsdt_addr: u64) -> Self {
        Self { xsdt_addr }
    }
}

impl Aml for Rsdp {
    fn to_aml_bytes(&self, sink: &mut dyn AmlSink) {
        rsdp::Rsdp::new(*OEM_ID, self.xsdt_addr).to_aml_bytes(sink);
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn field_references() {
        let mut sink = Vec::new();
        Rsdp::new(0x0abc_def0).to_aml_bytes(&mut sink);
        assert_eq!(
            sink[RSDP_XSDT_ADDR_OFFSET
                ..(RSDP_XSDT_ADDR_OFFSET + RSDP_XSDT_ADDR_LEN)],
            0x0abc_def0_u64.to_le_bytes()
        );
    }
}
