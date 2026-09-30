// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Generates an XSDT ACPI table for an instance.
//!
//! The [`Xsdt`] struct implements the `Aml` trait of the `acpi_tables` crate
//! and can write the AML bytecode to any AmlSink, like a `Vec<u8>`.

use super::{AcpiVariant, OEM_ID, OEM_REVISION, OEM_TABLE_ID};
use acpi_tables::{xsdt, Aml, AmlSink};

/// Byte offset and length of fields that need to be referenced during table
/// generation.
pub const XSDT_HEADER_LEN: usize = 36;

/// Configuration for generating a XSDT table.
pub struct XsdtConfig {
    /// The ACPI table variant to use.
    pub acpi_variant: AcpiVariant,

    /// 64-bit addresses of the linked tables.
    ///
    /// ACPI rev. 6.6 table 5.8 "Extended System Description Table Fields (XSDT)" Entry
    pub entries: Vec<u64>,
}

/// The XSDT table provides the addresses of additional tables.
///
/// ACPI rev. 6.6 section 5.2.8 "Extended System Description Table (XSDT)"
pub struct Xsdt {
    config: XsdtConfig,
}

impl Xsdt {
    pub fn new(config: XsdtConfig) -> Self {
        Self { config }
    }
}

impl Aml for Xsdt {
    // OVMF ignores the XSDT table loaded via fw_cfg and instead it generates
    // its own, so changes here will not appear to the guest when using OVMF.
    //
    // https://github.com/oxidecomputer/edk2/blob/f33871f488bfbbc080e0f7e3881e04d0db0b6367/OvmfPkg/AcpiPlatformDxe/QemuFwCfgAcpi.c#L891-L899
    fn to_aml_bytes(&self, sink: &mut dyn AmlSink) {
        let mut table = xsdt::XSDT::new(*OEM_ID, *OEM_TABLE_ID, OEM_REVISION);
        self.config.entries.iter().for_each(|e| table.add_entry(*e));
        table.to_aml_bytes(sink);
    }
}
