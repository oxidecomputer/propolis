// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::mem::size_of;

const ANCHOR: [u8; 4] = [b'_', b'S', b'M', b'_'];
const IANCHOR: [u8; 5] = [b'_', b'D', b'M', b'I', b'_'];

/// Each SMBIOS table is expected to terminate with a double-NUL
pub const TABLE_TERMINATOR: [u8; 2] = [0, 0];

/// SMBIOS Structure Table Entry Point
#[derive(Copy, Clone)]
#[repr(C, packed)]
pub(crate) struct EntryPoint {
    /// Anchor String (_SM_)
    pub anchor: [u8; 4],
    /// Entry Point Structure Checksum
    pub ep_checksum: u8,
    /// Entry Point Length
    pub ep_length: u8,
    /// SMBIOS Major Version
    pub major_version: u8,
    /// SMBIOS Minor Version
    pub minor_version: u8,
    /// Maximum Structure Size
    pub max_struct_sz: u16,
    /// Entry Point Revision
    pub ep_revision: u8,
    /// Formatted Area
    pub formatted_area: [u8; 5],
    /// Intermediate Anchor (_DMI_)
    pub intermed_anchor: [u8; 5],
    /// Intermediate Checksum
    pub intermed_checksum: u8,
    /// Structure Table Length
    pub table_length: u16,
    /// Structure Table Address
    pub table_address: u32,
    /// Number of SMBIOS Structures
    pub num_structs: u16,
    /// SMBIOS BCD Revision
    pub bcd_revision: u8,
}
impl EntryPoint {
    pub(crate) fn new() -> Self {
        Self {
            anchor: ANCHOR,
            ep_checksum: 0,
            ep_length: 0,
            major_version: 0,
            minor_version: 0,
            max_struct_sz: 0,
            ep_revision: 0,
            formatted_area: [0, 0, 0, 0, 0],
            intermed_anchor: IANCHOR,
            intermed_checksum: 0,
            table_length: 0,
            table_address: 0,
            num_structs: 0,
            bcd_revision: 0,
        }
    }
    pub(crate) fn update_cksums(&mut self) {
        self.ep_checksum = 0;
        self.intermed_checksum = 0;
        let isum = self.to_raw_bytes()[0x10..]
            .iter()
            .fold(0u8, |sum, item| sum.wrapping_add(*item));
        self.intermed_checksum = (!isum).wrapping_add(1);

        let sum = self
            .to_raw_bytes()
            .iter()
            .fold(0u8, |sum, item| sum.wrapping_add(*item));
        self.ep_checksum = (!sum).wrapping_add(1);
    }
    pub(crate) fn to_raw_bytes(&self) -> &[u8] {
        unsafe {
            std::slice::from_raw_parts(
                self as *const Self as *const u8,
                size_of::<Self>(),
            )
        }
    }
}

#[derive(Copy, Clone)]
#[repr(C, packed)]
pub(crate) struct StructureHeader {
    pub stype: u8,
    pub length: u8,
    pub handle: u16,
}

macro_rules! raw_table_impl {
    ($type_name:ident, $type_val:literal) => {
        unsafe impl RawTable for $type_name {
            const TYPE: u8 = $type_val;
            fn header_mut(&mut self) -> &mut StructureHeader {
                &mut self.header
            }
        }
    };
}

/// Type 0 (BIOS Information) - Version 2.7
#[derive(Copy, Clone)]
#[repr(C, packed)]
pub(crate) struct Type0 {
    pub header: StructureHeader,
    pub vendor: u8,
    pub bios_version: u8,
    pub bios_starting_seg_addr: u16,
    pub bios_release_date: u8,
    pub bios_rom_size: u8,
    pub bios_characteristics: u64,
    pub bios_ext_characteristics: u16,
    pub bios_major_release: u8,
    pub bios_minor_release: u8,
    pub ec_firmware_major_rel: u8,
    pub ec_firmware_minor_rel: u8,
}
raw_table_impl!(Type0, 0);

/// Type 1 (System Information) - Version 2.7
#[derive(Copy, Clone)]
#[repr(C, packed)]
pub(crate) struct Type1 {
    pub header: StructureHeader,
    pub manufacturer: u8,
    pub product_name: u8,
    pub version: u8,
    pub serial_number: u8,
    pub uuid: [u8; 16],
    pub wake_up_type: u8,
    pub sku_number: u8,
    pub family: u8,
}
raw_table_impl!(Type1, 1);

/// Type 2 (Baseboard Information)
#[derive(Copy, Clone)]
#[repr(C, packed)]
pub(crate) struct Type2 {
    pub header: StructureHeader,
    pub manufacturer: u8,
    pub product: u8,
    pub version: u8,
    pub serial_number: u8,
    pub asset_tag: u8,
    pub feature_flags: u8,
    pub location_in_chassis: u8,
    pub chassis_handle: u16,
    pub board_type: u8,
    pub number_obj_handles: u8,
    pub obj_handles: [u16; 0],
}
raw_table_impl!(Type2, 2);

/// Type 3 (System Enclosure) - Version 2.7
#[derive(Copy, Clone)]
#[repr(C, packed)]
pub(crate) struct Type3 {
    pub header: StructureHeader,
    pub manufacturer: u8,
    pub stype: u8,
    pub version: u8,
    pub serial_number: u8,
    pub asset_tag: u8,
    pub bootup_state: u8,
    pub psu_state: u8,
    pub thermal_state: u8,
    pub security_status: u8,
    pub oem_defined: u32,
    pub height: u8,
    pub num_cords: u8,
    pub contained_elem_count: u8,
    pub contained_elem_len: u8,
}
raw_table_impl!(Type3, 3);

/// Type 4 (Processor Information) - Version 2.7
#[derive(Copy, Clone)]
#[repr(C, packed)]
pub(crate) struct Type4 {
    pub header: StructureHeader,
    pub socket_designation: u8,
    pub proc_type: u8,
    pub proc_family: u8,
    pub proc_manufacturer: u8,
    pub proc_id: u64,
    pub proc_version: u8,
    pub voltage: u8,
    pub external_clock: u16,
    pub max_speed: u16,
    pub current_speed: u16,
    pub status: u8,
    pub proc_upgrade: u8,
    pub l1_cache_handle: u16,
    pub l2_cache_handle: u16,
    pub l3_cache_handle: u16,
    pub serial_number: u8,
    pub asset_tag: u8,
    pub part_number: u8,
    pub core_count: u8,
    pub core_enabled: u8,
    pub thread_count: u8,
    pub proc_characteristics: u16,
    pub proc_family2: u16,
}
raw_table_impl!(Type4, 4);

/// Type 16 (Physical Memory Array) - Version 2.7
#[derive(Copy, Clone)]
#[repr(C, packed)]
pub(crate) struct Type16 {
    pub header: StructureHeader,
    pub location: u8,
    pub array_use: u8,
    pub error_correction: u8,
    pub max_capacity: u32,
    pub error_info_handle: u16,
    pub num_mem_devices: u16,
    pub extended_max_capacity: u64,
}
raw_table_impl!(Type16, 16);

/// Type 17 (Memory Device) - Version 2.7
#[derive(Copy, Clone)]
#[repr(C, packed)]
pub(crate) struct Type17 {
    pub header: StructureHeader,
    pub phys_mem_array_handle: u16,
    pub mem_err_info_handle: u16,
    pub total_width: u16,
    pub data_width: u16,
    pub size: u16,
    pub form_factor: u8,
    pub device_set: u8,
    pub device_locator: u8,
    pub bank_locator: u8,
    pub memory_type: u8,
    pub type_detail: u16,
    pub speed: u16,
    pub manufacturer: u8,
    pub serial_number: u8,
    pub asset_tag: u8,
    pub part_number: u8,
    pub attributes: u8,
    pub extended_size: u32,
    pub cfgd_mem_clock_speed: u16,
    pub min_voltage: u16,
    pub max_voltage: u16,
    pub cfgd_voltage: u16,
}
raw_table_impl!(Type17, 17);

/// Type 32 (System Boot Information) - Version 2.7
#[derive(Copy, Clone)]
#[repr(C, packed)]
pub(crate) struct Type32 {
    pub header: StructureHeader,
    pub reserved: [u8; 6],
    pub boot_status: [u8; 0],
}
raw_table_impl!(Type32, 32);

/// Type 127 (End-of-Table)
#[derive(Copy, Clone)]
#[repr(C, packed)]
pub(crate) struct Type127 {
    pub header: StructureHeader,
}
raw_table_impl!(Type127, 127);

pub(crate) unsafe trait RawTable: Sized {
    const TYPE: u8;

    fn new(handle: u16) -> Self {
        // Safety: All of these structs are repr(C,packed) with no padding,
        // and thus safe to initialize as zeroed.
        let mut data =
            unsafe { std::mem::MaybeUninit::<Self>::zeroed().assume_init() };

        let header = data.header_mut();
        header.stype = Self::TYPE;
        header.length = size_of::<Self>() as u8;
        header.handle = handle;

        data
    }

    fn header_mut(&mut self) -> &mut StructureHeader;

    fn to_raw_bytes(&self) -> &[u8] {
        unsafe {
            std::slice::from_raw_parts(
                self as *const Self as *const u8,
                size_of::<Self>(),
            )
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use std::mem::size_of;

    #[test]
    fn entry_point() {
        assert_eq!(size_of::<EntryPoint>(), 0x1f);
    }
    #[test]
    fn struct_header() {
        assert_eq!(size_of::<StructureHeader>(), 0x4);
    }

    #[test]
    fn bios_information() {
        assert_eq!(size_of::<Type0>(), 0x18);
        let data = Type0::new(0xfffe);
        let data_raw = data.to_raw_bytes();
        assert_eq!(data_raw[0], 0, "type byte incorrect")
    }
    #[test]
    fn system_information() {
        assert_eq!(size_of::<Type1>(), 0x1b);
        let data = Type1::new(0xfffe);
        let data_raw = data.to_raw_bytes();
        assert_eq!(data_raw[0], 1, "type byte incorrect")
    }
    #[test]
    fn baseboard_information() {
        assert_eq!(size_of::<Type2>(), 0xf);
        let data = Type2::new(0xfffe);
        let data_raw = data.to_raw_bytes();
        assert_eq!(data_raw[0], 2, "type byte incorrect")
    }
    #[test]
    fn system_enclosure() {
        assert_eq!(size_of::<Type3>(), 0x15);
        let data = Type2::new(0xfffe);
        let data_raw = data.to_raw_bytes();
        assert_eq!(data_raw[0], 2, "type byte incorrect")
    }
    #[test]
    fn processor_information() {
        assert_eq!(size_of::<Type4>(), 0x2a);
        let data = Type3::new(0xfffe);
        let data_raw = data.to_raw_bytes();
        assert_eq!(data_raw[0], 3, "type byte incorrect")
    }
    #[test]
    fn physical_memory_array() {
        assert_eq!(size_of::<Type16>(), 0x17);
        let data = Type16::new(0xfffe);
        let data_raw = data.to_raw_bytes();
        assert_eq!(data_raw[0], 16, "type byte incorrect")
    }
    #[test]
    fn memory_device() {
        assert_eq!(size_of::<Type17>(), 0x28);
        let data = Type17::new(0xfffe);
        let data_raw = data.to_raw_bytes();
        assert_eq!(data_raw[0], 17, "type byte incorrect")
    }
    #[test]
    fn system_boot_information() {
        assert_eq!(size_of::<Type32>(), 0xa);
        let data = Type32::new(0xfffe);
        let data_raw = data.to_raw_bytes();
        assert_eq!(data_raw[0], 32, "type byte incorrect")
    }
}
