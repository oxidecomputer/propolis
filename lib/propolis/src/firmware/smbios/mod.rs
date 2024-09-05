// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::collections::BTreeMap;
use std::fmt;

use crate::cpuid;
use table::{Table, Type127};

mod bits;
pub mod table;

/// Collection of SMBIOS [table] instances, which will be rendered out
/// into two blocks of raw bytes representing the SMBIOS Entry Point and
/// SMBIOS Structure Table.
pub struct Tables {
    tables: BTreeMap<Handle, Vec<u8>>,
    total_size: usize,
    max_single_size: usize,
    eot_handle: Handle,
}
impl Tables {
    /// Create a new [Tables] collection.
    ///
    /// # Arguments
    /// - `eot_handle`: [Handle] for the automatically-added
    /// [end-of-table](table::Type127) structure, which must terminate the
    /// Structure Table.
    pub fn new(eot_handle: Handle) -> Self {
        let mut this = Self {
            tables: BTreeMap::new(),
            total_size: 0,
            max_single_size: 0,
            eot_handle,
        };
        this.add(eot_handle, &Type127::default()).unwrap();

        this
    }

    /// Add a [Table] to this collection
    ///
    /// # Arguments
    /// - `handle`: SMBIOS [Handle] to identify this table
    /// - `table`: [Table] to be added
    pub fn add(
        &mut self,
        handle: Handle,
        table: &dyn Table,
    ) -> Result<(), TableError> {
        let table_bytes = table.render(handle);
        let table_size = table_bytes.len();
        assert!(table_size != 0, "table should not be zero-length");

        if let Some(conflict) = self.tables.insert(handle, table_bytes) {
            // replace the item which we conflicted with in the first place
            let _ = self.tables.insert(handle, conflict);
            Err(TableError::HandleConflict(handle))
        } else {
            self.total_size += table_size;
            self.max_single_size = usize::max(self.max_single_size, table_size);
            Ok(())
        }
    }

    /// Build Entry Point structure.  Emits the raw-byte values of both the
    /// Entry Point and the associated Structure Table data.
    pub fn commit(self) -> TableBytes {
        let mut data = bits::EntryPoint::new();
        // hardcode to version 2.7 for now
        data.major_version = 2;
        data.minor_version = 7;
        data.bcd_revision = 0x27;
        data.table_length = self.total_size as u16;
        data.num_structs = self.tables.len() as u16;
        data.max_struct_sz = self.max_single_size as u16;
        data.update_cksums();

        let mut table_data = Vec::with_capacity(self.total_size);
        for (handle, table) in self.tables.iter() {
            // copy all non-EoT tables
            if *handle != self.eot_handle {
                table_data.extend_from_slice(table);
            }
        }
        // end-of-table goes at the end
        table_data.extend_from_slice(
            self.tables.get(&self.eot_handle).expect("EoT entry is present"),
        );

        TableBytes {
            entry_point: data.to_raw_bytes().to_vec(),
            structure_table: table_data,
        }
    }
}

pub struct TableBytes {
    pub entry_point: Vec<u8>,
    pub structure_table: Vec<u8>,
}

/// Possible errors when adding [Table] entries to [Tables]
#[derive(thiserror::Error, Debug)]
pub enum TableError {
    #[error("Conflicting handle {0}")]
    HandleConflict(Handle),
}

/// Structure Handle
///
/// A 16-bit number used to uniquely identify a single structure in a collection
/// of SMBIOS tables.
///
/// Defaults to 0xffff, which is considered "Unknown" in most SMBIOS tables.
#[derive(Copy, Clone, Eq, PartialEq, Hash, Debug, Ord, PartialOrd)]
#[repr(transparent)]
pub struct Handle(pub u16);
impl Handle {
    pub const UNKNOWN: Self = Self(0xffff);
}
impl fmt::Display for Handle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::LowerHex::fmt(&self.0, f)
    }
}
/// Default to the Unknown handle
impl Default for Handle {
    fn default() -> Self {
        Self::UNKNOWN
    }
}
impl From<u16> for Handle {
    fn from(value: u16) -> Self {
        Self(value)
    }
}
impl From<Handle> for u16 {
    fn from(value: Handle) -> Self {
        value.0
    }
}

/// SMBIOS-compatible string
///
/// Strings associated with an SMBIOS table are NUL-terminated, and concatenated
/// together directly following the formatted area of the table.  The
/// [tables](table) string data accept this type in order to expedite proper
/// formatting when they are rendered to raw bytes.
#[derive(Default, Clone)]
pub struct SmbString(Vec<u8>);
impl SmbString {
    pub const fn empty() -> Self {
        Self(Vec::new())
    }
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
    pub fn len(&self) -> usize {
        self.0.len()
    }
}
impl AsRef<[u8]> for SmbString {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}
impl TryFrom<Vec<u8>> for SmbString {
    type Error = SmbStringNulError;

    fn try_from(value: Vec<u8>) -> Result<Self, Self::Error> {
        if value.iter().any(|b| *b == 0) {
            Err(SmbStringNulError())
        } else {
            Ok(Self(value))
        }
    }
}
impl TryFrom<&str> for SmbString {
    type Error = SmbStringNulError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::try_from(value.to_owned().into_bytes())
    }
}
impl TryFrom<String> for SmbString {
    type Error = SmbStringNulError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::try_from(value.into_bytes())
    }
}

/// Error emitted when attempting to convert data bearing a NUL into a
/// [SmbString]
#[derive(thiserror::Error, Debug)]
#[error("String contains NUL byte")]
pub struct SmbStringNulError();

pub struct SmbiosParams {
    pub memory_size: usize,
    pub rom_size: usize,
    pub rom_release_date: String,
    pub rom_version: String,
    pub num_cpus: u8,
    pub cpuid_vendor: cpuid::Entry,
    pub cpuid_ident: cpuid::Entry,
    pub cpuid_procname: Option<[cpuid::Entry; 3]>,
    pub system_id: uuid::Uuid,
}

impl SmbiosParams {
    pub fn generate_table(&self) -> anyhow::Result<TableBytes> {
        use table::{type0, type1, type16, type4};
        let bios_version = self
            .rom_version
            .as_str()
            .try_into()
            .expect("bootrom version string doesn't contain NUL bytes");
        let smb_type0 = table::Type0 {
            vendor: "Oxide".try_into().unwrap(),
            bios_version,
            bios_release_date: self.rom_release_date.as_str().try_into().unwrap(),
            bios_rom_size: ((self.rom_size / (64 * 1024)) - 1) as u8,
            bios_characteristics: type0::BiosCharacteristics::UNSUPPORTED,
            bios_ext_characteristics: type0::BiosExtCharacteristics::ACPI
                | type0::BiosExtCharacteristics::UEFI
                | type0::BiosExtCharacteristics::IS_VM,
            ..Default::default()
        };

        let smb_type1 = table::Type1 {
            manufacturer: "Oxide".try_into().unwrap(),
            product_name: "OxVM".try_into().unwrap(),

            serial_number: self.system_id.to_string().try_into().unwrap_or_default(),
            uuid: self.system_id.to_bytes_le(),

            wake_up_type: type1::WakeUpType::PowerSwitch,
            ..Default::default()
        };

        let family = match self.cpuid_ident.eax & 0xf00 {
            // If family ID is 0xf, extended family is added to it
            0xf00 => (self.cpuid_ident.eax >> 20 & 0xff) + 0xf,
            // ... otherwise base family ID is used
            base => base >> 8,
        };

        let vendor = cpuid::VendorKind::try_from(self.cpuid_vendor);
        let proc_manufacturer = match vendor {
            Ok(cpuid::VendorKind::Intel) => "Intel",
            Ok(cpuid::VendorKind::Amd) => "Advanced Micro Devices, Inc.",
            _ => "",
        }
        .try_into()
        .unwrap();
        let proc_family = match (vendor, family) {
            // Zen
            (Ok(cpuid::VendorKind::Amd), family) if family >= 0x17 => 0x6b,
            //unknown
            _ => 0x2,
        };
        let proc_id = u64::from(self.cpuid_ident.eax) | u64::from(self.cpuid_ident.edx) << 32;
        let procname_entries = self.cpuid_procname.or_else(|| {
            if cpuid::host_query(cpuid::Ident(0x8000_0000, None)).eax >= 0x8000_0004
            {
                Some([
                    cpuid::host_query(cpuid::Ident(0x8000_0002, None)),
                    cpuid::host_query(cpuid::Ident(0x8000_0003, None)),
                    cpuid::host_query(cpuid::Ident(0x8000_0004, None)),
                ])
            } else {
                None
            }
        });
        let proc_version = procname_entries
            .and_then(|e| cpuid::parse_brand_string(e).ok())
            .unwrap_or("".to_string());

        let smb_type4 = table::Type4 {
            proc_type: type4::ProcType::Central,
            proc_family,
            proc_manufacturer,
            proc_id,
            proc_version: proc_version.as_str().try_into().unwrap_or_default(),
            status: type4::ProcStatus::Enabled,
            // unknown
            proc_upgrade: 0x2,
            // make core and thread counts equal for now
            core_count: self.num_cpus,
            core_enabled: self.num_cpus,
            thread_count: self.num_cpus,
            proc_characteristics: type4::Characteristics::IS_64_BIT
                | type4::Characteristics::MULTI_CORE,
            ..Default::default()
        };

        let mut smb_type16 = table::Type16 {
            location: type16::Location::SystemBoard,
            array_use: type16::ArrayUse::System,
            error_correction: type16::ErrorCorrection::Unknown,
            num_mem_devices: 1,
            ..Default::default()
        };
        smb_type16.set_max_capacity(self.memory_size);
        let phys_mem_array_handle = 0x1600.into();

        let mut smb_type17 = table::Type17 {
            phys_mem_array_handle,
            // Unknown
            form_factor: 0x2,
            // Unknown
            memory_type: 0x2,
            ..Default::default()
        };
        smb_type17.set_size(Some(self.memory_size));

        let smb_type32 = table::Type32::default();

        let mut smb_tables = Tables::new(0x7f00.into());
        smb_tables.add(0x0000.into(), &smb_type0).unwrap();
        smb_tables.add(0x0100.into(), &smb_type1).unwrap();
        smb_tables.add(0x0300.into(), &smb_type4).unwrap();
        smb_tables.add(phys_mem_array_handle, &smb_type16).unwrap();
        smb_tables.add(0x1700.into(), &smb_type17).unwrap();
        smb_tables.add(0x3200.into(), &smb_type32).unwrap();

        Ok(smb_tables.commit())
    }
}
