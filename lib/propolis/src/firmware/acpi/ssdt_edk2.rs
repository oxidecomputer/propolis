// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Generates the same SSDT table as the original EDK2 static tables.
//!
//! The [`Ssdt`] struct implements the `Aml` trait of the `acpi_tables` crate
//! and can write the AML bytecode to any AmlSink, like a `Vec<u8>`.

// This SSDT table is kept the same as the original EDK2 static table.
//
// https://github.com/oxidecomputer/edk2/blob/f33871f488bfbbc080e0f7e3881e04d0db0b6367/OvmfPkg/AcpiPlatformDxe/Qemu.c#L396
//
// This table can probably be removed in the future if the DSDT PCI0._CRS
// method is simplified.

use acpi_tables::{aml, sdt::Sdt, Aml, AmlSink};

fn ssdt_sdt_edk2_style() -> Sdt {
    // For SSDTs, the OEM table ID needs to be different for each table. Refer
    // to ACPI rev. 6.6 section 5.2.11.2 "Secondary System Description Table
    // (SSDT)" for more information.
    //
    // The SSDT provided by EDK2 can also be removed entirely in the future
    // because it only holds unsupported sleep states (S2 and S3) and FWDT
    // memory region.
    Sdt::new(*b"SSDT", 36, 1, *b"REDHAT", *b"OVMF    ", 0x1)
}

/// Length in bytes of the SSDT header. Used to calculate the offset of other
/// fields.
///
/// <https://uefi.org/htmlspecs/ACPI_Spec_6_4_html/05_ACPI_Software_Programming_Model/ACPI_Software_Programming_Model.html#secondary-system-description-table-fields-ssdt>
const SSDT_HEADER_LEN: usize = 36;

/// Byte offset of the FWDT OperationRegion offset address field in the SSDT
/// table. This field is updated in fwcfg.rs during table generation.
///
/// SSDT header (36 bytes) + External operation prefix (1 byte) +
/// OperationRegion prefix (1 byte) + OperationRegion name (4 bytes) +
/// OperationRegion space (1 byte) + DWordPrefix (1 byte)
///
/// <https://uefi.org/htmlspecs/ACPI_Spec_6_4_html/20_AML_Specification/AML_Specification.html#defopregion>
pub const FWDT_ADDR_OFFSET: usize = SSDT_HEADER_LEN + 8;

/// Number of bytes used to store the offset address value in the FWDT
/// OperationRegion. Size of a DWord.
pub const FWDT_ADDR_LEN: usize = 4;

/// Values for the PM1a_CNT.SLP_TYP register to enter different sleep states.
///
/// Transitions to S3 and S4 are inherited from the original EDK2 tables and
/// should probably be removed in the future since they are not handled by
/// Propolis.
const PM1A_CNT_SLP_TYP_S3: u8 = 1;
const PM1A_CNT_SLP_TYP_S4: u8 = 2;

/// The SSDT table is an extension to DSDT table and can be used to extend
/// resources defined in the DSDT.
///
/// <https://uefi.org/htmlspecs/ACPI_Spec_6_4_html/05_ACPI_Software_Programming_Model/ACPI_Software_Programming_Model.html#secondary-system-description-table-ssdt>
pub struct SsdtEdk2 {
    /// Offset of the area reserved for the FWDT OperationRegion data in the
    /// overall ACPI tables storage.
    fwdt_offset: usize,
}

impl SsdtEdk2 {
    pub fn new(fwdt_offset: usize) -> Self {
        Self { fwdt_offset }
    }
}

impl Aml for SsdtEdk2 {
    fn to_aml_bytes(&self, sink: &mut dyn AmlSink) {
        let mut ssdt = Vec::new();

        // The FWDT OperationRegion is used to pass dynamic information about
        // the instance from the platform to the virtual machine. The main
        // information provided are the 32-bit and 64-bit PCI MMIO ranges.
        //
        // On boot, the \_SB.PCI0._CRS method reads FWDT and adjusts the PCI
        // bus configuration based on the data it holds.
        //
        // This process can be removed if \_SB.PCI0._CRS is modified to return
        // a static ResourceTemplate that is already populated with the right
        // VM data.
        aml::OpRegion::new(
            "FWDT".into(),
            aml::OpRegionSpace::SystemMemory,
            &DWord::new(self.fwdt_offset as u32),
            &DWord::new(0x30),
        )
        .to_aml_bytes(&mut ssdt);

        // Sleep states.
        //
        // These sleep states are kept for consistency with the original static
        // EDK2 tables. Propolis doesn't handle these state properly, so they
        // should be removed in the future.
        //
        // These values don't use the SleepState struct to keep the generated
        // AML code the same as the original EDK tables.
        aml::Name::new(
            "\\_S3_".into(),
            &aml::Package::new(vec![
                &Byte::new(PM1A_CNT_SLP_TYP_S3),
                &Byte::new(0),
                &Byte::new(0),
                &Byte::new(0),
            ]),
        )
        .to_aml_bytes(&mut ssdt);

        aml::Name::new(
            "\\_S4_".into(),
            &aml::Package::new(vec![
                &Byte::new(PM1A_CNT_SLP_TYP_S4),
                &Byte::new(0),
                &Byte::new(0),
                &Byte::new(0),
            ]),
        )
        .to_aml_bytes(&mut ssdt);

        let mut sdt = ssdt_sdt_edk2_style();
        sdt.append_slice(ssdt.as_slice());
        sdt.to_aml_bytes(sink);
    }
}

// Provides consistent DWord AML values. The acpi_tables crate minimizes
// integers to the smallest word size that the number fits, but the original
// EDK2-defined ACPI tables used wider-than-necessary integers in some places.
//
// We retained this quirk to avoid unnecessary differences in ACPI tables when
// having Propolis generate them, but future ACPI table versions have no
// particular need to keep this.
struct DWord {
    value: u32,
}

impl DWord {
    fn new(value: u32) -> Self {
        Self { value }
    }
}

impl Aml for DWord {
    fn to_aml_bytes(&self, sink: &mut dyn AmlSink) {
        sink.byte(0x0c); // DWordPrefix
        sink.dword(self.value);
    }
}

// Provides consistent Byte AML values. The acpi_tables crate minimizes
// integers to the smallest word size that the number fits, but the original
// EDK2-defined ACPI tables used wider-than-necessary integers in some places.
//
// We retained this quirk to avoid unnecessary differences in ACPI tables when
// having Propolis generate them, but future ACPI table versions have no
// particular need to keep this.
struct Byte {
    value: u8,
}

impl Byte {
    fn new(value: u8) -> Self {
        Self { value }
    }
}

impl Aml for Byte {
    fn to_aml_bytes(&self, sink: &mut dyn AmlSink) {
        sink.byte(0x0a); // BytePrefix
        sink.byte(self.value);
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn field_references() {
        let mut sink = Vec::new();

        // Validate FWDT offset address field offset.
        SsdtEdk2::new(0xabc).to_aml_bytes(&mut sink);
        assert_eq!(
            sink[FWDT_ADDR_OFFSET..(FWDT_ADDR_OFFSET + FWDT_ADDR_LEN)],
            0xabc_u32.to_le_bytes()
        );
    }
}
