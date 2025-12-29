// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! ACPI table builders.
//!
//! This module provides builders for generating ACPI tables that are used
//! to describe the system configuration to guest firmware.

use std::mem::size_of;

use zerocopy::{Immutable, IntoBytes};

pub const ACPI_TABLE_HEADER_SIZE: usize = 36;
const ACPI_TABLE_LENGTH_OFFSET: usize = 4;

#[derive(Copy, Clone, IntoBytes, Immutable)]
#[repr(C, packed)]
struct AcpiTableHeader {
    signature: [u8; 4],
    length: u32,
    revision: u8,
    checksum: u8,
    oem_id: [u8; 6],
    oem_table_id: [u8; 8],
    oem_revision: u32,
    creator_id: [u8; 4],
    creator_revision: u32,
}

impl AcpiTableHeader {
    fn new(signature: [u8; 4], revision: u8) -> Self {
        Self {
            signature,
            length: 0,
            revision,
            checksum: 0,
            oem_id: *b"OXIDE\0",
            oem_table_id: *b"PROPOLIS",
            oem_revision: 1,
            creator_id: *b"OXDE",
            creator_revision: 1,
        }
    }
}

#[must_use = "call .finish() to get the table bytes"]
pub struct Rsdt {
    data: Vec<u8>,
}

impl Rsdt {
    pub fn new() -> Self {
        let header = AcpiTableHeader::new(*b"RSDT", 1);
        let mut data = vec![0u8; ACPI_TABLE_HEADER_SIZE];
        data[..ACPI_TABLE_HEADER_SIZE].copy_from_slice(header.as_bytes());
        Self { data }
    }

    pub fn add_entry(&mut self) -> u32 {
        let offset = self.data.len() as u32;
        self.data.extend_from_slice(&[0u8; size_of::<u32>()]);
        offset
    }

    pub fn finish(mut self) -> Vec<u8> {
        let length = self.data.len() as u32;
        self.data[ACPI_TABLE_LENGTH_OFFSET
            ..ACPI_TABLE_LENGTH_OFFSET + size_of::<u32>()]
            .copy_from_slice(&length.to_le_bytes());
        self.data
    }
}

#[must_use = "call .finish() to get the table bytes"]
pub struct Xsdt {
    data: Vec<u8>,
}

impl Xsdt {
    pub fn new() -> Self {
        let header = AcpiTableHeader::new(*b"XSDT", 1);
        let mut data = vec![0u8; ACPI_TABLE_HEADER_SIZE];
        data[..ACPI_TABLE_HEADER_SIZE].copy_from_slice(header.as_bytes());
        Self { data }
    }

    pub fn add_entry(&mut self) -> u32 {
        let offset = self.data.len() as u32;
        self.data.extend_from_slice(&[0u8; size_of::<u64>()]);
        offset
    }

    pub fn finish(mut self) -> Vec<u8> {
        let length = self.data.len() as u32;
        self.data[ACPI_TABLE_LENGTH_OFFSET
            ..ACPI_TABLE_LENGTH_OFFSET + size_of::<u32>()]
            .copy_from_slice(&length.to_le_bytes());
        self.data
    }
}

pub const RSDP_SIZE: usize = 36;
pub const RSDP_V1_SIZE: usize = 20;

const RSDP_SIGNATURE_OFFSET: usize = 0;
const RSDP_SIGNATURE_LEN: usize = 8;
pub const RSDP_CHECKSUM_OFFSET: usize = 8;
const RSDP_OEMID_OFFSET: usize = 9;
const RSDP_OEMID_LEN: usize = 6;
const RSDP_REVISION_OFFSET: usize = 15;
const RSDP_LENGTH_OFFSET: usize = 20;
pub const RSDP_XSDT_ADDR_OFFSET: usize = 24;
pub const RSDP_EXT_CHECKSUM_OFFSET: usize = 32;

#[must_use = "call .finish() to get the RSDP bytes"]
pub struct Rsdp {
    data: Vec<u8>,
}

impl Rsdp {
    pub fn new() -> Self {
        let mut data = vec![0u8; RSDP_SIZE];
        data[RSDP_SIGNATURE_OFFSET..RSDP_SIGNATURE_OFFSET + RSDP_SIGNATURE_LEN]
            .copy_from_slice(b"RSD PTR ");
        data[RSDP_OEMID_OFFSET..RSDP_OEMID_OFFSET + RSDP_OEMID_LEN]
            .copy_from_slice(b"OXIDE\0");
        data[RSDP_REVISION_OFFSET] = 2;
        data[RSDP_LENGTH_OFFSET..RSDP_LENGTH_OFFSET + size_of::<u32>()]
            .copy_from_slice(&(RSDP_SIZE as u32).to_le_bytes());
        Self { data }
    }

    pub fn finish(self) -> Vec<u8> {
        self.data
    }
}

pub struct Dsdt {
    data: Vec<u8>,
}

impl Dsdt {
    pub fn new() -> Self {
        let header = AcpiTableHeader::new(*b"DSDT", 2);
        let mut data = vec![0u8; ACPI_TABLE_HEADER_SIZE];
        data[..ACPI_TABLE_HEADER_SIZE].copy_from_slice(header.as_bytes());
        Self { data }
    }

    pub fn append_aml(&mut self, aml: &[u8]) {
        self.data.extend_from_slice(aml);
    }

    pub fn finish(mut self) -> Vec<u8> {
        let length = self.data.len() as u32;
        self.data[ACPI_TABLE_LENGTH_OFFSET
            ..ACPI_TABLE_LENGTH_OFFSET + size_of::<u32>()]
            .copy_from_slice(&length.to_le_bytes());
        self.data
    }
}

pub const FADT_SIZE: usize = 276;
pub const FADT_REVISION: u8 = 6;
pub const FADT_MINOR_REVISION: u8 = 5;

const FADT_FLAG_WBINVD: u32 = 1 << 0;
const FADT_FLAG_C1_SUPPORTED: u32 = 1 << 2;
const FADT_FLAG_SLP_BUTTON: u32 = 1 << 5;
const FADT_FLAG_TMR_VAL_EXT: u32 = 1 << 8;
const FADT_FLAG_RESET_REG_SUP: u32 = 1 << 10;
const FADT_FLAG_APIC_PHYSICAL: u32 = 1 << 19;
pub const FADT_FLAG_HW_REDUCED_ACPI: u32 = 1 << 20;

pub const FADT_OFF_FACS32: usize = 36;
pub const FADT_OFF_DSDT32: usize = 40;
pub const FADT_OFF_DSDT64: usize = 140;
const FADT_OFF_SCI_INT: usize = 46;
const FADT_OFF_PM1A_EVT_BLK: usize = 56;
const FADT_OFF_PM1A_CNT_BLK: usize = 64;
const FADT_OFF_PM_TMR_BLK: usize = 76;
const FADT_OFF_PM1_EVT_LEN: usize = 88;
const FADT_OFF_PM1_CNT_LEN: usize = 89;
const FADT_OFF_PM_TMR_LEN: usize = 91;
const FADT_OFF_IAPC_BOOT_ARCH: usize = 109;
const FADT_OFF_FLAGS: usize = 112;
const FADT_OFF_RESET_REG: usize = 116;
const FADT_OFF_RESET_VALUE: usize = 128;
const FADT_OFF_MINOR_REV: usize = 131;
const FADT_OFF_X_PM1A_EVT_BLK: usize = 148;
const FADT_OFF_X_PM1A_CNT_BLK: usize = 172;
const FADT_OFF_X_PM_TMR_BLK: usize = 208;
const FADT_OFF_HYPERVISOR_ID: usize = 268;

const GAS_OFF_SPACE_ID: usize = 0;
const GAS_OFF_BIT_WIDTH: usize = 1;
const GAS_OFF_ACCESS_WIDTH: usize = 3;
const GAS_OFF_ADDRESS: usize = 4;
const GAS_ADDRESS_LEN: usize = 8;
const GAS_SPACE_SYSTEM_IO: u8 = 1;
const GAS_ACCESS_BYTE: u8 = 1;
const GAS_ACCESS_WORD: u8 = 2;
const GAS_ACCESS_DWORD: u8 = 3;

const ACPI_RESET_REG_PORT: u64 = 0xcf9;
const ACPI_RESET_VALUE: u8 = 0x06;

const IAPC_BOOT_ARCH_LEGACY_DEVICES: u16 = 1 << 0;
const IAPC_BOOT_ARCH_8042: u16 = 1 << 1;

const PIIX4_PM_BASE: u32 = 0xb000;
const PIIX4_PM1A_CNT_OFF: u32 = 4;
const PIIX4_PM_TMR_OFF: u32 = 8;
const PIIX4_PM1_EVT_LEN: u8 = 4;
const PIIX4_PM1_CNT_LEN: u8 = 2;
const PIIX4_PM_TMR_LEN: u8 = 4;
const PIIX4_SCI_IRQ: u16 = 9;

const HYPERVISOR_ID: &[u8] = b"OXIDE";

pub struct Fadt {
    data: Vec<u8>,
}

impl Fadt {
    pub fn new() -> Self {
        let header = AcpiTableHeader::new(*b"FACP", FADT_REVISION);
        let mut data = vec![0u8; FADT_SIZE];
        data[..ACPI_TABLE_HEADER_SIZE].copy_from_slice(header.as_bytes());
        data[ACPI_TABLE_LENGTH_OFFSET
            ..ACPI_TABLE_LENGTH_OFFSET + size_of::<u32>()]
            .copy_from_slice(&(FADT_SIZE as u32).to_le_bytes());

        data[FADT_OFF_SCI_INT..FADT_OFF_SCI_INT + size_of::<u16>()]
            .copy_from_slice(&PIIX4_SCI_IRQ.to_le_bytes());

        data[FADT_OFF_PM1A_EVT_BLK..FADT_OFF_PM1A_EVT_BLK + size_of::<u32>()]
            .copy_from_slice(&PIIX4_PM_BASE.to_le_bytes());
        data[FADT_OFF_PM1A_CNT_BLK..FADT_OFF_PM1A_CNT_BLK + size_of::<u32>()]
            .copy_from_slice(&(PIIX4_PM_BASE + PIIX4_PM1A_CNT_OFF).to_le_bytes());
        data[FADT_OFF_PM_TMR_BLK..FADT_OFF_PM_TMR_BLK + size_of::<u32>()]
            .copy_from_slice(&(PIIX4_PM_BASE + PIIX4_PM_TMR_OFF).to_le_bytes());

        data[FADT_OFF_PM1_EVT_LEN] = PIIX4_PM1_EVT_LEN;
        data[FADT_OFF_PM1_CNT_LEN] = PIIX4_PM1_CNT_LEN;
        data[FADT_OFF_PM_TMR_LEN] = PIIX4_PM_TMR_LEN;

        let boot_arch = IAPC_BOOT_ARCH_LEGACY_DEVICES | IAPC_BOOT_ARCH_8042;
        data[FADT_OFF_IAPC_BOOT_ARCH
            ..FADT_OFF_IAPC_BOOT_ARCH + size_of::<u16>()]
            .copy_from_slice(&boot_arch.to_le_bytes());

        let flags = FADT_FLAG_WBINVD
            | FADT_FLAG_C1_SUPPORTED
            | FADT_FLAG_SLP_BUTTON
            | FADT_FLAG_TMR_VAL_EXT
            | FADT_FLAG_RESET_REG_SUP
            | FADT_FLAG_APIC_PHYSICAL;
        data[FADT_OFF_FLAGS..FADT_OFF_FLAGS + size_of::<u32>()]
            .copy_from_slice(&flags.to_le_bytes());

        data[FADT_OFF_RESET_REG + GAS_OFF_SPACE_ID] = GAS_SPACE_SYSTEM_IO;
        data[FADT_OFF_RESET_REG + GAS_OFF_BIT_WIDTH] = u8::BITS as u8;
        data[FADT_OFF_RESET_REG + GAS_OFF_ACCESS_WIDTH] = GAS_ACCESS_BYTE;
        data[FADT_OFF_RESET_REG + GAS_OFF_ADDRESS
            ..FADT_OFF_RESET_REG + GAS_OFF_ADDRESS + GAS_ADDRESS_LEN]
            .copy_from_slice(&ACPI_RESET_REG_PORT.to_le_bytes());
        data[FADT_OFF_RESET_VALUE] = ACPI_RESET_VALUE;

        data[FADT_OFF_MINOR_REV] = FADT_MINOR_REVISION;

        data[FADT_OFF_X_PM1A_EVT_BLK + GAS_OFF_SPACE_ID] = GAS_SPACE_SYSTEM_IO;
        data[FADT_OFF_X_PM1A_EVT_BLK + GAS_OFF_BIT_WIDTH] = PIIX4_PM1_EVT_LEN * 8;
        data[FADT_OFF_X_PM1A_EVT_BLK + GAS_OFF_ACCESS_WIDTH] = GAS_ACCESS_DWORD;
        data[FADT_OFF_X_PM1A_EVT_BLK + GAS_OFF_ADDRESS
            ..FADT_OFF_X_PM1A_EVT_BLK + GAS_OFF_ADDRESS + GAS_ADDRESS_LEN]
            .copy_from_slice(&(PIIX4_PM_BASE as u64).to_le_bytes());

        data[FADT_OFF_X_PM1A_CNT_BLK + GAS_OFF_SPACE_ID] = GAS_SPACE_SYSTEM_IO;
        data[FADT_OFF_X_PM1A_CNT_BLK + GAS_OFF_BIT_WIDTH] = PIIX4_PM1_CNT_LEN * 8;
        data[FADT_OFF_X_PM1A_CNT_BLK + GAS_OFF_ACCESS_WIDTH] = GAS_ACCESS_WORD;
        data[FADT_OFF_X_PM1A_CNT_BLK + GAS_OFF_ADDRESS
            ..FADT_OFF_X_PM1A_CNT_BLK + GAS_OFF_ADDRESS + GAS_ADDRESS_LEN]
            .copy_from_slice(
                &((PIIX4_PM_BASE + PIIX4_PM1A_CNT_OFF) as u64).to_le_bytes(),
            );

        data[FADT_OFF_X_PM_TMR_BLK + GAS_OFF_SPACE_ID] = GAS_SPACE_SYSTEM_IO;
        data[FADT_OFF_X_PM_TMR_BLK + GAS_OFF_BIT_WIDTH] = PIIX4_PM_TMR_LEN * 8;
        data[FADT_OFF_X_PM_TMR_BLK + GAS_OFF_ACCESS_WIDTH] = GAS_ACCESS_DWORD;
        data[FADT_OFF_X_PM_TMR_BLK + GAS_OFF_ADDRESS
            ..FADT_OFF_X_PM_TMR_BLK + GAS_OFF_ADDRESS + GAS_ADDRESS_LEN]
            .copy_from_slice(
                &((PIIX4_PM_BASE + PIIX4_PM_TMR_OFF) as u64).to_le_bytes(),
            );

        data[FADT_OFF_HYPERVISOR_ID..FADT_OFF_HYPERVISOR_ID + HYPERVISOR_ID.len()]
            .copy_from_slice(HYPERVISOR_ID);
        Self { data }
    }

    pub fn new_reduced() -> Self {
        let mut fadt = Self::new();
        fadt.data[FADT_OFF_FLAGS..FADT_OFF_FLAGS + size_of::<u32>()]
            .copy_from_slice(&FADT_FLAG_HW_REDUCED_ACPI.to_le_bytes());
        fadt
    }

    pub fn finish(self) -> Vec<u8> {
        self.data
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic() {
        let mut xsdt = Xsdt::new();
        xsdt.add_entry();
        let xsdt_data = xsdt.finish();
        assert_eq!(&xsdt_data[0..4], b"XSDT");

        let rsdp = Rsdp::new();
        let rsdp_data = rsdp.finish();
        assert_eq!(&rsdp_data[0..8], b"RSD PTR ");

        let dsdt = Dsdt::new();
        let dsdt_data = dsdt.finish();
        assert_eq!(&dsdt_data[0..4], b"DSDT");

        let fadt = Fadt::new();
        let fadt_data = fadt.finish();
        assert_eq!(&fadt_data[0..4], b"FACP");
    }
}
