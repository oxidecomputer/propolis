// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::collections::BTreeMap;
use std::fmt;

use table::{Table, Type127};

mod bits;
pub mod table;

/// Collection of SMBIOS [table](table) instances, which will be rendered out
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
