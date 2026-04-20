// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Generates a FADT/FACP ACPI table for an instance.
//!
//! The [`Fadt`] struct implements the `Aml` trait of the `acpi_tables` crate
//! and can write the AML bytecode to any AmlSink, like a `Vec<u8>`.

use super::{OEM_ID, OEM_REVISION, OEM_TABLE_ID, SCI_IRQ};
use acpi_tables::{
    // XXX(acpi): Use version 3 to keep FADT table consistent with the original
    //            EKD2 static tables. The acpi_tables crate also generates the
    //            MADT table using revision 1, which fwts reports not being
    //            compatible with FADT 6.5.
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
const PM1A_EVT_BLK_ADDR: u32 = 0xb000;
const PM1A_CNT_BLK_ADDR: u32 = 0xb004;
const PM_TMR_BLK_ADDR: u32 = 0xb008;
const GPE0_BLK_ADDR: u32 = 0xafe0;

const PM1A_EVT_BLK_LEN: u8 = 4;
const PM1A_CNT_BLK_LEN: u8 = 2;
const PM_TMR_BLK_LEN: u8 = 4;
const GPE0_BLK_LEN: u8 = 4;

// Represent a bit flag for the FADT IA-PC boot architecture flags.
//
// <https://uefi.org/htmlspecs/ACPI_Spec_6_4_html/05_ACPI_Software_Programming_Model/ACPI_Software_Programming_Model.html#ia-pc-boot-architecture-flags>
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

/// The FADT table stores fixed hardware ACPI information.
///
/// <https://uefi.org/htmlspecs/ACPI_Spec_6_4_html/05_ACPI_Software_Programming_Model/ACPI_Software_Programming_Model.html#fixed-acpi-description-table-fadt>
pub struct Fadt {
    facs_offset: u32,
    dsdt_offset: u32,
}

impl Fadt {
    pub fn new(facs_offset: u32, dsdt_offset: u32) -> Self {
        Self { facs_offset, dsdt_offset }
    }
}

// XXX(acpi): Values retained from the original EDK2 static tables.
//            fwts reports 1 high failure for this table:
//              - fadt: FADT X_GPE0_BLK Access width 0x00 but it should be 1 (byte access).
//
// https://github.com/oxidecomputer/edk2/blob/f33871f488bfbbc080e0f7e3881e04d0db0b6367/OvmfPkg/AcpiTables/Platform.h#L25-L56
impl Aml for Fadt {
    fn to_aml_bytes(&self, sink: &mut dyn AmlSink) {
        let mut fadt = FADTBuilder::new(*OEM_ID, *OEM_TABLE_ID, OEM_REVISION)
            .firmware_ctrl_32(self.facs_offset)
            .dsdt_32(self.dsdt_offset)
            .dsdt_64(self.dsdt_offset as u64)
            .flag(Flags::Wbinvd)
            .flag(Flags::ProcC1)
            .flag(Flags::SlpButton)
            .flag(Flags::RtcS4)
            .flag(Flags::TmrValExt)
            .flag(Flags::ResetRegSup);

        fadt.sci_int = (SCI_IRQ as u16).into();
        fadt.smi_cmd = 0xb2.into();
        fadt.acpi_enable = 0xf1.into();
        fadt.acpi_disable = 0xf0.into();

        fadt.pm1a_evt_blk = PM1A_EVT_BLK_ADDR.into();
        fadt.pm1a_cnt_blk = PM1A_CNT_BLK_ADDR.into();
        fadt.pm_tmr_blk = PM_TMR_BLK_ADDR.into();
        fadt.gpe0_blk = GPE0_BLK_ADDR.into();

        fadt.pm1_evt_len = PM1A_EVT_BLK_LEN.into();
        fadt.pm1_cnt_len = PM1A_CNT_BLK_LEN.into();
        fadt.pm_tmr_len = PM_TMR_BLK_LEN.into();
        fadt.gpe0_blk_len = GPE0_BLK_LEN.into();

        fadt.p_lvl2_lat = 101.into();
        fadt.p_lvl3_lat = 1001.into();

        let iapc_boot_arch = FadtIaPcBootArchFlags::empty();
        fadt.iapc_boot_arch = iapc_boot_arch.bits().into();

        fadt.reset_reg = GAS::new(
            AddressSpace::SystemIo,
            u8::BITS as u8,
            0,
            AccessSize::Undefined,
            0x0cf9,
        );
        fadt.reset_value = 0x06;

        fadt.x_pm1a_evt_blk = GAS::new(
            AddressSpace::SystemIo,
            PM1A_EVT_BLK_LEN * 8,
            0,
            AccessSize::Undefined,
            PM1A_EVT_BLK_ADDR as u64,
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
