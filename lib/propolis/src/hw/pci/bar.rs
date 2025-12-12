// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::convert::TryFrom;
use std::convert::TryInto;

use crate::migrate::MigrateStateError;

use super::bits;
use super::BarN;

pub const BAR_COUNT: usize = 6;

#[derive(Eq, PartialEq, Clone, Copy, Debug)]
pub enum BarDefine {
    Pio(u16),
    Mmio(u32),
    Mmio64(u64),
}
impl BarDefine {
    /// Definition represent PIO-backed BAR
    pub fn is_pio(&self) -> bool {
        matches!(self, BarDefine::Pio(_))
    }
    /// Definition represent MMIO-backed (32-bit or 64-bit) BAR
    pub fn is_mmio(&self) -> bool {
        matches!(self, BarDefine::Mmio(_) | BarDefine::Mmio64(_))
    }
    /// Get the size of the BAR definition, regardless of type
    pub fn size(&self) -> u64 {
        match self {
            BarDefine::Pio(sz) => u64::from(*sz),
            BarDefine::Mmio(sz) => u64::from(*sz),
            BarDefine::Mmio64(sz) => *sz,
        }
    }
}
impl TryFrom<EntryKind> for BarDefine {
    type Error = ();

    fn try_from(value: EntryKind) -> Result<Self, Self::Error> {
        match value {
            EntryKind::Empty | EntryKind::Mmio64High => Err(()),
            EntryKind::Pio(sz) => Ok(BarDefine::Pio(sz)),
            EntryKind::Mmio(sz) => Ok(BarDefine::Mmio(sz)),
            EntryKind::Mmio64(sz) => Ok(BarDefine::Mmio64(sz)),
        }
    }
}

#[derive(Copy, Clone)]
enum EntryKind {
    Empty,
    Pio(u16),
    Mmio(u32),
    Mmio64(u64),
    Mmio64High,
}

#[derive(Copy, Clone)]
struct Entry {
    kind: EntryKind,
    value: u64,
}
impl Default for Entry {
    fn default() -> Self {
        Self { kind: EntryKind::Empty, value: 0 }
    }
}

pub(super) struct Bars {
    entries: [Entry; BAR_COUNT],
}

impl Bars {
    pub(super) fn new(defs: &[Option<BarDefine>; BAR_COUNT]) -> Self {
        let mut this = Self { entries: Default::default() };
        for (idx, def) in defs.iter().enumerate() {
            match def {
                None => continue,
                Some(def) => {
                    assert!(matches!(this.entries[idx].kind, EntryKind::Empty));
                    this.entries[idx].kind = match def {
                        BarDefine::Pio(sz) => EntryKind::Pio(*sz),
                        BarDefine::Mmio(sz) => EntryKind::Mmio(*sz),
                        BarDefine::Mmio64(sz) => {
                            // Make sure 64-bit BAR definitions are playing by
                            // the rules
                            assert!(idx < (BarN::BAR5 as usize));
                            this.entries[idx + 1].kind = EntryKind::Mmio64High;
                            EntryKind::Mmio64(*sz)
                        }
                    }
                }
            }
        }

        this
    }
    pub(super) fn reg_read(&self, bar: BarN) -> u32 {
        let idx = bar as usize;
        let ent = self.entries[idx];
        match ent.kind {
            EntryKind::Empty => 0,
            EntryKind::Pio(_) => {
                u32::from(ent.value as u16) | bits::BAR_TYPE_IO
            }
            EntryKind::Mmio(_) => ent.value as u32 | bits::BAR_TYPE_MEM,
            EntryKind::Mmio64(_) => ent.value as u32 | bits::BAR_TYPE_MEM64,
            EntryKind::Mmio64High => {
                assert_ne!(idx, 0);
                let ent = self.entries[idx - 1];
                assert!(matches!(ent.kind, EntryKind::Mmio64(_)));

                (ent.value >> 32) as u32
            }
        }
    }
    pub(super) fn reg_write(
        &mut self,
        bar: BarN,
        val: u32,
    ) -> Option<WriteResult> {
        let idx = bar as usize;
        let ent = &mut self.entries[idx];
        let (id, def, val_old, val_new) = match ent.kind {
            EntryKind::Empty => return None,
            EntryKind::Pio(size) => {
                let mask = u32::from(!(size - 1));
                let old = ent.value;
                ent.value = u64::from(val & mask);
                (bar, BarDefine::Pio(size), old, ent.value)
            }
            EntryKind::Mmio(size) => {
                let mask = !(size - 1);
                let old = ent.value;
                ent.value = u64::from(val & mask);
                (bar, BarDefine::Mmio(size), old, ent.value)
            }
            EntryKind::Mmio64(size) => {
                let old = ent.value;
                let mask = !(size - 1) as u32;
                let low = val & mask;
                ent.value = (old & (0xffffffff << 32)) | u64::from(low);
                (bar, BarDefine::Mmio64(size), old, ent.value)
            }
            EntryKind::Mmio64High => {
                assert!(idx > 0);
                let real_idx = idx - 1;
                let id = BarN::from_repr(real_idx as u8).unwrap();
                let ent = &mut self.entries[real_idx];
                let size = match ent.kind {
                    EntryKind::Mmio64(sz) => sz,
                    _ => panic!(),
                };
                let mask = !(size - 1);
                let old = ent.value;
                let high = ((u64::from(val) << 32) & mask) & 0xffffffff00000000;
                ent.value = high | (old & 0xffffffff);
                (id, BarDefine::Mmio64(size), old, ent.value)
            }
        };
        if val_old != val_new {
            return Some(WriteResult { id, def, val_old, val_new });
        }
        None
    }

    /// Get BAR definition and current value
    pub fn get(&self, n: BarN) -> Option<(BarDefine, u64)> {
        let ent = &self.entries[n as usize];
        let def = BarDefine::try_from(ent.kind).ok()?;
        Some((def, ent.value))
    }

    /// Set BAR value directly
    ///
    /// May only be called on BARs which are defined (not on empty BARs or the
    /// high portions of 64-bit MMIO BARs).  Furthermore, the value must be
    /// valid for the BAR type  (ie. not > u32::MAX for 32-bit MMIO BAR).
    pub fn set(&mut self, n: BarN, value: u64) {
        let ent = &mut self.entries[n as usize];
        match ent.kind {
            EntryKind::Empty => panic!("{:?} not defined", n),
            EntryKind::Mmio64High => {
                panic!("high BAR bits not to be set directly")
            }
            EntryKind::Pio(_) => {
                assert!(value <= u64::from(u16::MAX));
                ent.value = value;
            }
            EntryKind::Mmio(_) => {
                assert!(value <= u64::from(u32::MAX));
            }
            EntryKind::Mmio64(_) => {}
        }
        ent.value = value;
    }

    pub(super) fn export(&self) -> migrate::BarStateV1 {
        let entries = self.entries.map(|entry| match entry.kind {
            EntryKind::Pio(sz) => migrate::BarEntryV1 {
                kind: migrate::BarKindV1::Pio,
                size: u64::from(sz),
                value: entry.value,
            },
            EntryKind::Mmio(sz) => migrate::BarEntryV1 {
                kind: migrate::BarKindV1::Mmio,
                size: u64::from(sz),
                value: entry.value,
            },
            EntryKind::Mmio64(sz) => migrate::BarEntryV1 {
                kind: migrate::BarKindV1::Mmio64,
                size: sz,
                value: entry.value,
            },
            // We encode `Mmio64High` as Empty here because it is always implied
            // by a preceding `Mmio64` entry.
            EntryKind::Mmio64High | EntryKind::Empty => migrate::BarEntryV1 {
                kind: migrate::BarKindV1::Empty,
                size: 0,
                value: 0,
            },
        });
        migrate::BarStateV1 { entries }
    }

    pub(super) fn import(
        &mut self,
        input_bars: migrate::BarStateV1,
    ) -> Result<(), MigrateStateError> {
        let mut entries = self.entries.iter_mut();
        let mut input_entries = IntoIterator::into_iter(input_bars.entries);

        while let (Some(entry), Some(input_entry)) =
            (entries.next(), input_entries.next())
        {
            let sz = input_entry.size;
            entry.kind = match input_entry.kind {
                migrate::BarKindV1::Empty => EntryKind::Empty,
                migrate::BarKindV1::Pio => {
                    let sz = sz.try_into().map_err(|_| {
                        MigrateStateError::ImportFailed(format!(
                            "Pio Bar: invalid entry size ({})",
                            sz
                        ))
                    })?;
                    EntryKind::Pio(sz)
                }
                migrate::BarKindV1::Mmio => {
                    let sz = sz.try_into().map_err(|_| {
                        MigrateStateError::ImportFailed(format!(
                            "Mmio Bar: invalid entry size ({})",
                            sz
                        ))
                    })?;
                    EntryKind::Mmio(sz)
                }
                migrate::BarKindV1::Mmio64 => {
                    // An `Mmio64` already implies the next should be `Mmio64High` so
                    // the export logic just leaves the slot empty.
                    match input_entries.next() {
                        Some(e) if e.kind == migrate::BarKindV1::Empty => {
                            // Verify there's an available next slot that will be
                            // set to Mmio64High
                            let next_entry = entries
                                .next()
                                .ok_or_else(|| {
                                    MigrateStateError::ImportFailed(
                                        "Mmio64 Bar: last entry cannot be set as Mmio64"
                                        .to_string()
                                    )
                                })?;
                            next_entry.kind = EntryKind::Mmio64High;
                        }
                        _ => return Err(MigrateStateError::ImportFailed(
                            "Mmio64 Bar: expected empty entry for Mmio64High"
                                .to_string(),
                        )),
                    }
                    EntryKind::Mmio64(sz)
                }
            };
            entry.value = input_entry.value;
        }

        Ok(())
    }
}

/// Result from a write to a BAR
pub struct WriteResult {
    /// Identifier of the actual impacted BAR.
    ///
    /// If write was to the high word of a 64-bit BAR, this would hold the
    /// `BarN` for the lower word.
    pub id: BarN,
    pub def: BarDefine,
    pub val_old: u64,
    pub val_new: u64,
}

pub mod migrate {
    use serde::{Deserialize, Serialize};

    use super::BAR_COUNT;

    #[derive(Eq, PartialEq, Deserialize, Serialize)]
    pub enum BarKindV1 {
        Empty,
        Pio,
        Mmio,
        Mmio64,
    }

    #[derive(Deserialize, Serialize)]
    pub struct BarEntryV1 {
        pub kind: BarKindV1,
        pub size: u64,
        pub value: u64,
    }

    #[derive(Deserialize, Serialize)]
    pub struct BarStateV1 {
        pub entries: [BarEntryV1; BAR_COUNT],
    }
}

#[cfg(test)]
mod test {
    use super::*;

    fn setup() -> Bars {
        let bar_defs = [
            Some(BarDefine::Pio(0x100)),
            Some(BarDefine::Mmio(0x20000)),
            Some(BarDefine::Mmio64(0x40000)),
            None, // high bits
            Some(BarDefine::Mmio64(0x200000000)),
            None, // high bits
        ];
        let bars = Bars::new(&bar_defs);

        bars
    }
    #[test]
    fn init() {
        let _ = setup();
    }

    #[test]
    fn read_type() {
        let mut bars = setup();
        bars.set(BarN::BAR0, 0x1000);
        bars.set(BarN::BAR1, 0xc000000);
        bars.set(BarN::BAR2, 0xd000000);
        bars.set(BarN::BAR4, 0x800000000);

        assert_eq!(bars.reg_read(BarN::BAR0), 0x1001);
        assert_eq!(bars.reg_read(BarN::BAR1), 0xc000000);
        assert_eq!(bars.reg_read(BarN::BAR2), 0xd000004);
        assert_eq!(bars.reg_read(BarN::BAR3), 0);
        assert_eq!(bars.reg_read(BarN::BAR4), 0x4);
        assert_eq!(bars.reg_read(BarN::BAR5), 0x8);
    }

    #[test]
    fn write_place() {
        let mut bars = setup();
        bars.reg_write(BarN::BAR0, 0x1000);
        bars.reg_write(BarN::BAR1, 0xc000000);
        bars.reg_write(BarN::BAR2, 0xd000000);
        bars.reg_write(BarN::BAR5, 0x8);
        bars.reg_write(BarN::BAR4, 0x0);

        assert_eq!(bars.reg_read(BarN::BAR0), 0x1001);
        assert_eq!(bars.reg_read(BarN::BAR1), 0xc000000);
        assert_eq!(bars.reg_read(BarN::BAR2), 0xd000004);
        assert_eq!(bars.reg_read(BarN::BAR3), 0);
        assert_eq!(bars.reg_read(BarN::BAR4), 0x4);
        assert_eq!(bars.reg_read(BarN::BAR5), 0x8);
    }

    #[test]
    fn limits() {
        let mut bars = setup();
        for i in 0..=5u8 {
            bars.reg_write(BarN::from_repr(i).unwrap(), 0xffffffff);
        }
        assert_eq!(bars.reg_read(BarN::BAR0), 0x0000ff01);
        assert_eq!(bars.reg_read(BarN::BAR1), 0xfffe0000);
        assert_eq!(bars.reg_read(BarN::BAR2), 0xfffc0004);
        assert_eq!(bars.reg_read(BarN::BAR3), 0xffffffff);
        assert_eq!(bars.reg_read(BarN::BAR4), 0x00000004);
        assert_eq!(bars.reg_read(BarN::BAR5), 0xfffffffe);
    }
}
