// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use crate::common::*;
use crate::firmware::smbios::bits::{self, RawTable};
use crate::firmware::smbios::{Handle, SmbString};

pub trait Table {
    fn render(&self, handle: Handle) -> Vec<u8>;
}

#[derive(Default)]
pub struct Type0 {
    pub vendor: SmbString,
    pub bios_version: SmbString,
    pub bios_starting_seg_addr: u16,
    pub bios_release_date: SmbString,
    pub bios_rom_size: u8,
    pub bios_characteristics: u64,
    pub bios_ext_characteristics: u16,
    pub bios_major_release: u8,
    pub bios_minor_release: u8,
    pub ec_firmware_major_rel: u8,
    pub ec_firmware_minor_rel: u8,
}
impl Table for Type0 {
    fn render(&self, handle: Handle) -> Vec<u8> {
        let mut stab = StringTable::new();
        let data = bits::Type0 {
            vendor: stab.add(&self.vendor),
            bios_version: stab.add(&self.bios_version),
            bios_starting_seg_addr: self.bios_starting_seg_addr,
            bios_release_date: stab.add(&self.bios_release_date),
            bios_rom_size: self.bios_rom_size,
            bios_characteristics: self.bios_characteristics,
            bios_ext_characteristics: self.bios_ext_characteristics,
            bios_major_release: self.bios_major_release,
            bios_minor_release: self.bios_minor_release,
            ec_firmware_major_rel: self.ec_firmware_major_rel,
            ec_firmware_minor_rel: self.ec_firmware_minor_rel,
            ..bits::Type0::new(handle.into())
        };

        render_table(data, None, Some(stab))
    }
}

#[derive(Default)]
pub struct Type1 {
    pub manufacturer: SmbString,
    pub product_name: SmbString,
    pub version: SmbString,
    pub serial_number: SmbString,
    pub uuid: [u8; 16],
    pub wake_up_type: u8,
    pub sku_number: SmbString,
    pub family: SmbString,
}
impl Table for Type1 {
    fn render(&self, handle: Handle) -> Vec<u8> {
        let mut stab = StringTable::new();
        let data = bits::Type1 {
            manufacturer: stab.add(&self.manufacturer),
            product_name: stab.add(&self.product_name),
            version: stab.add(&self.version),
            serial_number: stab.add(&self.serial_number),
            uuid: self.uuid,
            wake_up_type: self.wake_up_type,
            sku_number: stab.add(&self.sku_number),
            family: stab.add(&self.family),
            ..bits::Type1::new(handle.into())
        };

        render_table(data, None, Some(stab))
    }
}

#[derive(Default)]
pub struct Type4 {
    pub socket_designation: SmbString,
    pub proc_type: u8,
    pub proc_family: u8,
    pub proc_manufacturer: SmbString,
    pub proc_id: u64,
    pub proc_version: SmbString,
    pub voltage: u8,
    pub external_clock: u16,
    pub max_speed: u16,
    pub current_speed: u16,
    pub status: u8,
    pub proc_upgrade: u8,
    pub l1_cache_handle: Handle,
    pub l2_cache_handle: Handle,
    pub l3_cache_handle: Handle,
    pub serial_number: SmbString,
    pub asset_tag: SmbString,
    pub part_number: SmbString,
    pub core_count: u8,
    pub core_enabled: u8,
    pub thread_count: u8,
    pub proc_characteristics: u16,
    pub proc_family2: u16,
}
impl Type4 {
    pub fn set_family(&mut self, family: u16) {
        if family > 0xff {
            self.proc_family = 0xfe;
            self.proc_family2 = family;
        } else {
            self.proc_family = family as u8;
            self.proc_family2 = 0;
        }
    }
}
impl Table for Type4 {
    fn render(&self, handle: Handle) -> Vec<u8> {
        let mut stab = StringTable::new();
        let data = bits::Type4 {
            socket_designation: stab.add(&self.socket_designation),
            proc_type: self.proc_type,
            proc_family: self.proc_family,
            proc_manufacturer: stab.add(&self.proc_manufacturer),
            proc_id: self.proc_id,
            proc_version: stab.add(&self.proc_version),
            voltage: self.voltage,
            external_clock: self.external_clock,
            max_speed: self.max_speed,
            current_speed: self.current_speed,
            status: self.status,
            proc_upgrade: self.proc_upgrade,
            l1_cache_handle: self.l1_cache_handle.into(),
            l2_cache_handle: self.l2_cache_handle.into(),
            l3_cache_handle: self.l3_cache_handle.into(),
            serial_number: stab.add(&self.serial_number),
            asset_tag: stab.add(&self.asset_tag),
            part_number: stab.add(&self.part_number),
            core_count: self.core_count,
            core_enabled: self.core_enabled,
            thread_count: self.thread_count,
            proc_characteristics: self.proc_characteristics,
            proc_family2: self.proc_family2,
            ..bits::Type4::new(handle.into())
        };
        render_table(data, None, Some(stab))
    }
}

#[derive(Default)]
pub struct Type16 {
    pub location: u8,
    pub array_use: u8,
    pub error_correction: u8,
    pub max_capacity: u32,
    pub error_info_handle: Handle,
    pub num_mem_devices: u16,
    pub extended_max_capacity: u64,
}
impl Type16 {
    pub fn set_max_capacity(&mut self, capacity_bytes: usize) {
        let capacity_kib = capacity_bytes / KB;

        if capacity_bytes >= (2 * TB) {
            self.max_capacity = 0x8000_0000;
            self.extended_max_capacity = capacity_kib as u64;
        } else {
            self.max_capacity = capacity_kib as u32;
        }
    }
}
impl Table for Type16 {
    fn render(&self, handle: Handle) -> Vec<u8> {
        let data = bits::Type16 {
            location: self.location,
            array_use: self.array_use,
            error_correction: self.error_correction,
            max_capacity: self.max_capacity,
            error_info_handle: self.error_info_handle.into(),
            num_mem_devices: self.num_mem_devices,
            extended_max_capacity: self.extended_max_capacity,
            ..bits::Type16::new(handle.into())
        };
        render_table(data, None, None)
    }
}

#[derive(Default)]
pub struct Type17 {
    pub phys_mem_array_handle: Handle,
    pub mem_err_info_handle: Handle,
    pub total_width: u16,
    pub data_width: u16,
    pub size: u16,
    pub form_factor: u8,
    pub device_set: u8,
    pub device_locator: SmbString,
    pub bank_locator: SmbString,
    pub memory_type: u8,
    pub type_detail: u16,
    pub speed: u16,
    pub manufacturer: SmbString,
    pub serial_number: SmbString,
    pub asset_tag: SmbString,
    pub part_number: SmbString,
    pub attributes: u8,
    pub extended_size: u32,
    pub cfgd_mem_clock_speed: u16,
    pub min_voltage: u16,
    pub max_voltage: u16,
    pub cfgd_voltage: u16,
}
impl Type17 {
    pub fn set_size(&mut self, size_bytes: Option<usize>) {
        match size_bytes {
            None => {
                self.size = 0xffff;
                self.extended_size = 0;
            }
            // size <= 32GiB - 1MiB does not need extended_size
            Some(n) if n < (32767 * MB) => {
                self.size = (n / MB) as u16;
            }
            Some(n) => {
                self.size = 0x7fff;
                self.extended_size = (n / MB) as u32;
            }
        }
    }
}
impl Table for Type17 {
    fn render(&self, handle: Handle) -> Vec<u8> {
        let mut stab = StringTable::new();
        let data = bits::Type17 {
            phys_mem_array_handle: self.phys_mem_array_handle.into(),
            mem_err_info_handle: self.mem_err_info_handle.into(),
            total_width: self.total_width,
            data_width: self.data_width,
            size: self.size,
            form_factor: self.form_factor,
            device_set: self.device_set,
            device_locator: stab.add(&self.device_locator),
            bank_locator: stab.add(&self.bank_locator),
            memory_type: self.memory_type,
            type_detail: self.type_detail,
            speed: self.speed,
            manufacturer: stab.add(&self.manufacturer),
            serial_number: stab.add(&self.serial_number),
            asset_tag: stab.add(&self.asset_tag),
            part_number: stab.add(&self.part_number),
            attributes: self.attributes,
            extended_size: self.extended_size,
            cfgd_mem_clock_speed: self.cfgd_mem_clock_speed,
            min_voltage: self.min_voltage,
            max_voltage: self.max_voltage,
            cfgd_voltage: self.cfgd_voltage,
            ..bits::Type17::new(handle.into())
        };

        render_table(data, None, Some(stab))
    }
}

#[derive(Default)]
pub struct Type32();
impl Table for Type32 {
    fn render(&self, handle: Handle) -> Vec<u8> {
        let data = bits::Type32::new(handle.into());

        // Boot status code for "no errors detected"
        let boot_status = [0u8];

        render_table(data, Some(&boot_status), None)
    }
}

#[derive(Default)]
pub struct Type127();
impl Table for Type127 {
    fn render(&self, handle: Handle) -> Vec<u8> {
        bits::Type127::new(handle.into()).to_raw_bytes().into()
    }
}

/// Render all components of a SMBIOS table into raw bytes
///
/// # Arguments
/// - `raw_table`: [RawTable] instance representing the structure
/// - `extra_data`: Any data belonging in the formatted area of the structure
///   which is not already covered by its fields (variable length additions)
/// - `stab`: [StringTable] of any associated strings
fn render_table(
    mut raw_table: impl RawTable,
    extra_data: Option<&[u8]>,
    stab: Option<StringTable>,
) -> Vec<u8> {
    let extra_data = extra_data.unwrap_or(&[]);

    if extra_data.len() > 0 {
        let header = raw_table.header_mut();
        header.length = header
            .length
            .checked_add(extra_data.len() as u8)
            .expect("extra data does not overflow length");
    }
    let raw_data = raw_table.to_raw_bytes();

    // non-generic render, for when raw_table has been turned into bytes
    fn _render_table(
        raw_data: &[u8],
        extra_data: &[u8],
        stab: Option<StringTable>,
    ) -> Vec<u8> {
        let stab_data = stab.and_then(|stab| stab.render());

        let term_len = stab_data
            .as_ref()
            .map(|s| s.len())
            .unwrap_or(bits::TABLE_TERMINATOR.len());

        let mut buf =
            Vec::with_capacity(raw_data.len() + extra_data.len() + term_len);
        buf.extend_from_slice(raw_data);
        buf.extend_from_slice(extra_data);
        if let Some(stab) = stab_data {
            buf.extend_from_slice(&stab);
        } else {
            buf.extend_from_slice(&bits::TABLE_TERMINATOR);
        }
        buf
    }

    _render_table(raw_data, extra_data, stab)
}

struct StringTable<'a> {
    strings: Vec<&'a SmbString>,
    len_with_nulls: usize,
}
impl<'a> StringTable<'a> {
    fn new() -> Self {
        Self { strings: Vec::new(), len_with_nulls: 0 }
    }
    /// Add a [SmbString] to the [StringTable], emitting its index value for
    /// inclusion in the structure to which it is being associated.
    fn add(&mut self, data: &'a SmbString) -> u8 {
        if data.is_empty() {
            0u8
        } else {
            assert!(self.strings.len() < 254);
            self.len_with_nulls += data.len() + 1;
            self.strings.push(data);
            let idx = self.strings.len() as u8;

            idx
        }
    }
    /// Render associated strings raw bytes, properly formatted to be appended
    /// to an associated SMBIOS table.  Returns `None` if no strings were added
    /// to the table.
    fn render(mut self) -> Option<Vec<u8>> {
        if self.strings.is_empty() {
            None
        } else {
            let mut out = Vec::with_capacity(self.len_with_nulls + 1);
            for string in self.strings.drain(..) {
                out.extend_from_slice(string.as_ref());
                out.push(b'\0');
            }
            // table expected to end with double-NUL
            out.push(b'\0');
            Some(out)
        }
    }
}
