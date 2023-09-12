// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use serde::{Serialize, Deserialize};

/// IO port and IRQ definitions for standard IBM PC hardware

pub const PORT_COM1: u16 = 0x3f8;
pub const PORT_COM2: u16 = 0x2f8;
pub const PORT_COM3: u16 = 0x3e8;
pub const PORT_COM4: u16 = 0x2e8;
pub const IRQ_COM1: u8 = 4;
pub const IRQ_COM2: u8 = 3;
pub const IRQ_COM3: u8 = 4;
pub const IRQ_COM4: u8 = 3;

pub const PORT_FAST_A20: u16 = 0x92;
pub const PORT_POST_CODE: u16 = 0x80;
pub const LEN_FAST_A20: u16 = 1;
pub const LEN_POST_CODE: u16 = 1;

pub const PORT_PS2_DATA: u16 = 0x60;
pub const PORT_PS2_CMD_STATUS: u16 = 0x64;
pub const IRQ_PS2_PRI: u8 = 1;
pub const IRQ_PS2_AUX: u8 = 12;

pub const PORT_ATA0_CMD: u16 = 0x1f0;
pub const PORT_ATA1_CMD: u16 = 0x170;
pub const PORT_ATA0_CTRL: u16 = 0x3f6;
pub const PORT_ATA1_CTRL: u16 = 0x376;
pub const LEN_ATA_CMD: u16 = 8;
pub const LEN_ATA_CTRL: u16 = 2;
pub const IRQ_ATA0: u8 = 14;
pub const IRQ_ATA1: u8 = 15;

/// MS-DOS Master Boot Record types

/// Offsets of the Partition Entry structs in the MBR.
pub const MBR_PARTITION_ENTRY_OFFSET: [usize; 4] = [446, 462, 478, 494];

/// MS-DOS MBR Partition Entry
#[derive(Serialize, Deserialize, Debug)]
pub struct PartitionEntry {
    status: u8,
    first_sector_chs: [u8; 3],
    partition_type: u8,
    last_sector_chs: [u8; 3],
    first_sector_lba: u32,
    sectors: u32,
}

/// Check whether or not the given byte slice contains a master boot record.
/// This assumes the length of the slice is at least 512 bytes.
pub fn check_ms_dos_mbr_signature(mbr: &[u8]) -> bool {
    mbr[510] == 0x55 && mbr[511] == 0xaa
}
