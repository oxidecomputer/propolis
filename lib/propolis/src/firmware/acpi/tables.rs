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
    }
}
