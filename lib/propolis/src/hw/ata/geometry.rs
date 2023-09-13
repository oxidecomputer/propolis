// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use slog::{Record, Result, Serializer, KV};

/// A distinct type to hold the number of sectors of a disk drive.
#[derive(Copy, Clone, Default, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Sectors(u64);

impl Sectors {
    pub const ONE_KB: Sectors = Self(2);
    pub const ONE_MB: Sectors = Self(1024 * Self::ONE_KB.0);
    pub const ONE_GB: Sectors = Self(1024 * Self::ONE_MB.0);

    #[inline]
    pub fn new(size: u64) -> Self {
        Sectors(size)
    }

    #[inline]
    pub fn lba28(&self) -> u32 {
        self.0 as u32 & 0x0fffffff
    }

    #[inline]
    pub fn lba28_low(&self) -> u16 {
        self.lba28() as u16
    }

    #[inline]
    pub fn lba28_high(&self) -> u16 {
        (self.lba28() >> 16) as u16
    }

    #[inline]
    pub fn lba48_low(&self) -> u16 {
        self.0 as u16
    }

    #[inline]
    pub fn lba48_mid(&self) -> u16 {
        (self.0 >> 16) as u16
    }

    #[inline]
    pub fn lba48_high(&self) -> u16 {
        (self.0 >> 32) as u16
    }
}

impl std::ops::Mul<u64> for Sectors {
    type Output = Self;

    fn mul(self, rhs: u64) -> Self {
        Self(self.0 * rhs)
    }
}

impl std::ops::Mul<Sectors> for u64 {
    type Output = Sectors;

    fn mul(self, rhs: Sectors) -> Sectors {
        rhs * self
    }
}

/// A struct holding geometry information for a disk drive. Some helpers are
/// defined to compute these fields.
#[derive(Copy, Clone, Default, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Geometry {
    pub cylinders: u16,
    pub heads: u8,
    pub sectors: u8,
}

impl Geometry {
    pub const MIN: Self = Self { cylinders: 1, heads: 1, sectors: 1 };
    pub const MAX: Self = Self { cylinders: u16::MAX, heads: 16, sectors: 255 };
    pub const MAX_COMPAT: Self =
        Self { cylinders: 16383, heads: 15, sectors: 63 };

    #[inline]
    pub fn capacity(&self) -> Sectors {
        Sectors(self.cylinders as u64 * self.heads as u64 * self.sectors as u64)
    }

    #[inline]
    pub fn max_logical_block_address(&self) -> LogicalBlockAddress {
        LogicalBlockAddress(self.capacity().0)
    }

    pub fn from_lba(
        &self,
        lba: LogicalBlockAddress,
    ) -> CylinderHeadSectorAddress {
        CylinderHeadSectorAddress::MIN
    }

    pub fn compute_cylinders(&mut self, capacity: Sectors) {
        let sectors_per_cylinder = self.sectors as u32 * self.heads as u32;

        self.cylinders = if capacity.lba28() > 16514064 {
            (16514064 / sectors_per_cylinder) as u16
        } else {
            (capacity.lba28() / sectors_per_cylinder) as u16
        }
    }

    // pub fn guess_from_master_boot_record(mbr: &[u8]) -> Result<Self, AtaError> {
    //     if !check_ms_dos_mbr_signature(mbr) {
    //         return Err(AtaError::NoMasterBootRecord);
    //     }

    //     // For any valid partition entries, determine the number of cylinders
    //     // occupied by that partition.
    //     let cylinders_per_partition = MBR_PARTITION_ENTRY_OFFSET.iter().map(|offset| {
    //         let entry = bincode2::deserialize::<PartitionEntry>(mbr[offset..]).unwrap();

    //         if entry.status != 0x0 {
    //             let first_address = CylinderHeadSectorAddress::from_mbr_bytes(&entry.first_sector_chs);
    //             let last_address = CylinderHeadSectorAddress::from_mbr_bytes(&entry.last_sector_chs);
    //             let cylinders = last_address.cylinders() - first_address.cylinders() + 1;

    //             Some((cylinders, entry.sectors))
    //         } else {
    //             None
    //         }
    //     });
    // }
}

impl KV for Geometry {
    fn serialize(
        &self,
        _rec: &Record,
        serializer: &mut dyn Serializer,
    ) -> Result {
        serializer.emit_u16("cylinders", self.cylinders)?;
        serializer.emit_u8("heads", self.heads)?;
        serializer.emit_u8("sectors", self.sectors)
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct CylinderHeadSectorAddress {
    pub cylinder: u16,
    pub head: u8,
    pub sector: u8,
}

impl CylinderHeadSectorAddress {
    const MIN: Self = Self { cylinder: 0, head: 0, sector: 1 };
    const MAX: Self = Self { cylinder: 1023, head: 255, sector: 63 };

    pub fn from_mbr_bytes(bytes: &[u8]) -> Self {
        let cylinder = ((bytes[1] & 0xc) as u16) << 2 | bytes[2] as u16;
        let head = bytes[0];
        let sector = bytes[1] & 0x3f;

        Self { cylinder, head, sector }
    }

    pub fn logical_block_address(
        &self,
        geometry: &Geometry,
    ) -> LogicalBlockAddress {
        LogicalBlockAddress(
            (((self.cylinder as u64 * geometry.heads as u64)
                + self.head as u64)
                * geometry.sectors as u64)
                + self.sector as u64
                - 1u64,
        )
    }
}

impl Default for CylinderHeadSectorAddress {
    fn default() -> Self {
        Self::MIN
    }
}

#[derive(Copy, Clone, Default, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct LogicalBlockAddress(u64);
