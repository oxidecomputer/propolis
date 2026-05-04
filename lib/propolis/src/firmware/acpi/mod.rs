// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! ACPI table and AML bytecode generation.

pub mod dsdt;
pub mod facs;
pub mod fadt;
pub mod madt;
pub mod rsdp;
pub mod xsdt;

pub use dsdt::{
    Dsdt, DsdtConfig, DsdtGenerator, DsdtScope, Ssdt, SSDT_FWDT_ADDR_LEN,
    SSDT_FWDT_ADDR_OFFSET,
};
pub use facs::Facs;
pub use fadt::{
    Fadt, FADT_DSDT_LEN, FADT_DSDT_OFFSET, FADT_FACS_LEN, FADT_FACS_OFFSET,
    FADT_X_DSDT_LEN, FADT_X_DSDT_OFFSET,
};
pub use madt::{Madt, MadtConfig};
pub use rsdp::{
    Rsdp, RSDP_EXTENDED_CHECKSUM_OFFSET, RSDP_EXTENDED_TABLE_LEN,
    RSDP_V1_CHECKSUM_OFFSET, RSDP_V1_TABLE_LEN, RSDP_XSDT_ADDR_LEN,
    RSDP_XSDT_ADDR_OFFSET,
};
pub use xsdt::{Xsdt, XSDT_HEADER_LEN};

// Values used to reference table checksums to recompute them after values are
// changed during table generation.
pub const TABLE_HEADER_CHECKSUM_OFFSET: usize = 9;
pub const TABLE_HEADER_CHECKSUM_LEN: usize = 1;

// Internal values shared across tables.

// XXX(acpi): Values inherited from the original EDK2 static tables. They could
//            be set to Propolis-specific values in the future.
const OEM_ID: &[u8; 6] = b"OVMF  ";
const OEM_TABLE_ID: &[u8; 8] = b"OVMFEDK2";
const OEM_REVISION: u32 = 0x20130221;

const SCI_IRQ: u8 = 0x09;
const PCI_LINK_IRQS: [u8; 4] = [0x05, SCI_IRQ, 0x0a, 0x0b];

const IO_APIC_ADDR: u32 = 0xfec0_0000;
const LOCAL_APIC_ADDR: u32 = 0xfee0_0000;

const PM1A_EVT_BLK_ADDR: u16 = 0xb000;

const GPE0_BLK_ADDR: u16 = 0xafe0;
const GPE0_BLK_LEN: u8 = 4;
