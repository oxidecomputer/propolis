// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Generates a FADT/FACP ACPI table for an instance.
//!
//! The [`Fadt`] struct implements the `Aml` trait of the `acpi_tables` crate
//! and can write the AML bytecode to any AmlSink, like a `Vec<u8>`.

use super::{
    AcpiVariant, GPE0_BLK_ADDR, GPE0_BLK_LEN, OEM_ID, OEM_REVISION,
    OEM_TABLE_ID,
};
use crate::hw::{chipset::i440fx, pci};
use acpi_tables::{
    // Use version 3 to keep FADT table consistent with the original EDK2
    // static tables. The acpi_tables crate also generates the MADT table using
    // revision 1, which is only compatible with FADT up to version 3.
    //
    // https://github.com/fwts/fwts/blob/3e05ba9c2640a85cac1f408a423d25e712677fa1/src/acpi/madt/madt.c#L30
    fadt_3::{FADTBuilder, Flags},
    gas::{AccessSize, AddressSpace, GAS},
    Aml,
    AmlSink,
};

// Byte offset and length of fields that need to be referenced during table
// generation.
pub const FADT_FACS_OFFSET: usize = 36;
pub const FADT_FACS_LEN: usize = 4;

pub const FADT_DSDT_OFFSET: usize = 40;
pub const FADT_DSDT_LEN: usize = 4;

pub const FADT_X_DSDT_OFFSET: usize = 140;
pub const FADT_X_DSDT_LEN: usize = 8;

// Values used to populate the FADT table.
const PM1A_CNT_BLK_ADDR: u16 = i440fx::PMBASE_DEFAULT + 0x04;
const PM_TMR_BLK_ADDR: u16 = i440fx::PMBASE_DEFAULT + 0x08;

const PM1A_EVT_BLK_LEN: u8 = 4;
const PM1A_CNT_BLK_LEN: u8 = 2;
const PM_TMR_BLK_LEN: u8 = 4;

// Represents a bit flag for the FADT IA-PC boot architecture flags.
//
// ACPI rev. 6.6 section 5.2.9.3 "IA-PC Boot Architecture Flags"
bitflags! {
    pub struct FadtIaPcBootArchFlags: u16 {
        const LEGACY_DEVICES = 1 << 0;
        const ARCH_8042 = 1 << 1;
        const VGA_NOT_PRESENT = 1 << 2;
        const MSI_NOT_SUPPORTED = 1 << 3;
        const PCIE_ASPM_CONTROLS = 1 << 4;
        const CMOS_RTC_NOT_PRESENT = 1 << 5;
    }
}

/// Configuration for generating a FADT table.
pub struct FadtConfig {
    /// The ACPI table variant to use.
    pub acpi_variant: AcpiVariant,

    /// Memory address for the FACS table.
    ///
    /// ACPI rev. 6.6 table 5.9 "FADT Format" "FIRMWARE_CTRL"
    pub fwctrl_addr: u32,

    /// 32-bit memory address for the DSDT table.
    ///
    /// ACPI rev. 6.6 table 5.9 "FADT Format" "DSDT"
    pub dsdt_addr: u32,

    /// 64-bit memory address for the DSDT table.
    ///
    /// ACPI rev. 6.6 table 5.9 "FADT Format" "X_DSDT"
    pub x_dsdt_addr: u64,
}

/// The FADT table stores fixed hardware ACPI information.
///
/// ACPI rev. 6.6 section 5.2.9 "Fixed ACPI Description Table (FADT)"
pub struct Fadt {
    config: FadtConfig,
}

impl Fadt {
    pub fn new(config: FadtConfig) -> Self {
        Self { config }
    }
}

impl Aml for Fadt {
    /// Generates AML code for the FADT table. Refer to ACPI specification sec.
    /// 5.2.9 "Fixed ACPI Description Table (FADT)" for more information about
    /// the individual fields.
    ///
    /// The current values are retained from the original EDK2 static tables.
    /// https://github.com/oxidecomputer/edk2/blob/f33871f488bfbbc080e0f7e3881e04d0db0b6367/OvmfPkg/AcpiTables/Platform.h#L25-L56
    ///
    /// fwts reports 1 high failure for this table:
    ///   - fadt: FADT X_GPE0_BLK Access width 0x00 but it should be 1 (byte access).
    fn to_aml_bytes(&self, sink: &mut dyn AmlSink) {
        let mut fadt = FADTBuilder::new(*OEM_ID, *OEM_TABLE_ID, OEM_REVISION)
            .firmware_ctrl_32(self.config.fwctrl_addr)
            .dsdt_32(self.config.dsdt_addr)
            .dsdt_64(self.config.x_dsdt_addr)
            .flag(Flags::Wbinvd)
            .flag(Flags::ProcC1)
            .flag(Flags::SlpButton)
            .flag(Flags::RtcS4)
            .flag(Flags::TmrValExt)
            .flag(Flags::ResetRegSup);

        fadt.sci_int = (i440fx::SCI_IRQ as u16).into();
        if self.config.acpi_variant == AcpiVariant::V0 {
            // Propolis doesn't currently handle this I/O port, but its value
            // is retained from the original EDK2 tables for consistency. It
            // should be set to zero in the future to disable System Management
            // mode.
            fadt.smi_cmd = 0xb2.into();
        }
        fadt.acpi_enable = 0xf1;
        fadt.acpi_disable = 0xf0;

        fadt.pm1a_evt_blk = (i440fx::PMBASE_DEFAULT as u32).into();
        fadt.pm1a_cnt_blk = (PM1A_CNT_BLK_ADDR as u32).into();
        fadt.pm_tmr_blk = (PM_TMR_BLK_ADDR as u32).into();
        fadt.gpe0_blk = (GPE0_BLK_ADDR as u32).into();

        fadt.pm1_evt_len = PM1A_EVT_BLK_LEN;
        fadt.pm1_cnt_len = PM1A_CNT_BLK_LEN;
        fadt.pm_tmr_len = PM_TMR_BLK_LEN;
        fadt.gpe0_blk_len = GPE0_BLK_LEN;

        // Disable C2 support. From the ACPI spec.:
        // "A value > 100 indicates the system does not support a C2 state."
        fadt.p_lvl2_lat = 101.into();

        // Disable C3 support. From the ACPI spec.:
        // "A value > 1000 indicates the system does not support a C3 state."
        fadt.p_lvl3_lat = 1001.into();

        let iapc_boot_arch = FadtIaPcBootArchFlags::empty();
        fadt.iapc_boot_arch = iapc_boot_arch.bits().into();

        fadt.reset_reg = GAS::new(
            AddressSpace::SystemIo,
            u8::BITS as u8,
            0,
            AccessSize::Undefined,
            pci::bits::PORT_ACPI_RESET_ADDR,
        );
        fadt.reset_value = pci::bits::PORT_ACPI_RESET_VALUE;

        fadt.x_pm1a_evt_blk = GAS::new(
            AddressSpace::SystemIo,
            PM1A_EVT_BLK_LEN * 8,
            0,
            AccessSize::Undefined,
            i440fx::PMBASE_DEFAULT as u64,
        );
        fadt.x_pm1a_cnt_blk = GAS::new(
            AddressSpace::SystemIo,
            PM1A_CNT_BLK_LEN * 8,
            0,
            AccessSize::Undefined,
            PM1A_CNT_BLK_ADDR as u64,
        );

        fadt.x_pm_tmr_blk = GAS::new(
            AddressSpace::SystemIo,
            PM_TMR_BLK_LEN * 8,
            0,
            AccessSize::Undefined,
            PM_TMR_BLK_ADDR as u64,
        );

        fadt.x_gpe0_blk = GAS::new(
            AddressSpace::SystemIo,
            GPE0_BLK_LEN * 8,
            0,
            AccessSize::Undefined,
            GPE0_BLK_ADDR as u64,
        );

        fadt.finalize().to_aml_bytes(sink);
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn field_references() {
        let mut sink = Vec::new();
        let config = FadtConfig {
            acpi_variant: AcpiVariant::V0,
            fwctrl_addr: 0x0abc,
            dsdt_addr: 0x0def,
            x_dsdt_addr: 0x0def,
        };
        Fadt::new(config).to_aml_bytes(&mut sink);
        assert_eq!(
            sink[FADT_FACS_OFFSET..(FADT_FACS_OFFSET + FADT_FACS_LEN)],
            0x0abc_u32.to_le_bytes()
        );
        assert_eq!(
            sink[FADT_DSDT_OFFSET..(FADT_DSDT_OFFSET + FADT_DSDT_LEN)],
            0x0000_u32.to_le_bytes() // Calling dsdt_64 zeros the 32-bit field.
        );
        assert_eq!(
            sink[FADT_X_DSDT_OFFSET..(FADT_X_DSDT_OFFSET + FADT_X_DSDT_LEN)],
            0x0def_u64.to_le_bytes()
        );
    }
}
