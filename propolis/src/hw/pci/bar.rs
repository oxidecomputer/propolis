use std::convert::TryFrom;

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
            BarDefine::Pio(sz) => *sz as u64,
            BarDefine::Mmio(sz) => *sz as u64,
            BarDefine::Mmio64(sz) => *sz as u64,
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
            EntryKind::Pio(_) => (ent.value as u16) as u32 | bits::BAR_TYPE_IO,
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
    ) -> Option<(BarDefine, u64, u64)> {
        let idx = bar as usize;
        let ent = &mut self.entries[idx];
        let (def, old, new) = match ent.kind {
            EntryKind::Empty => return None,
            EntryKind::Pio(size) => {
                let mask = !(size - 1) as u32;
                let old = ent.value;
                ent.value = (val & mask) as u64;
                (BarDefine::Pio(size), old, ent.value)
            }
            EntryKind::Mmio(size) => {
                let mask = !(size - 1);
                let old = ent.value;
                ent.value = (val & mask) as u64;
                (BarDefine::Mmio(size), old, ent.value)
            }
            EntryKind::Mmio64(size) => {
                let old = ent.value;
                let mask = !(size - 1) as u32;
                let low = val as u32 & mask;
                ent.value = (old & (0xffffffff << 32)) | low as u64;
                (BarDefine::Mmio64(size), old, ent.value)
            }
            EntryKind::Mmio64High => {
                assert!(idx > 0);
                let mut ent = &mut self.entries[idx - 1];
                let size = match ent.kind {
                    EntryKind::Mmio64(sz) => sz,
                    _ => panic!(),
                };
                let mask = !(size - 1);
                let old = ent.value;
                let high = (((val as u64) << 32) & mask) & 0xffffffff00000000;
                ent.value = high | (old & 0xffffffff);
                (BarDefine::Mmio64(size), old, ent.value)
            }
        };
        if old != new {
            return Some((def, old, new));
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
                assert!(value <= u16::MAX as u64);
                ent.value = value;
            }
            EntryKind::Mmio(_) => {
                assert!(value <= u32::MAX as u64);
            }
            EntryKind::Mmio64(_) => {}
        }
        ent.value = value;
    }

    pub(super) fn export(&self) -> migrate::BarStateV1 {
        let mut entries = Vec::new();
        for (idx, entry) in self.entries.iter().enumerate() {
            match entry.kind {
                EntryKind::Pio(sz) => entries.push(migrate::BarEntryV1 {
                    n: idx as u8,
                    kind: migrate::BarKindV1::Pio,
                    size: sz as u64,
                    value: entry.value,
                }),
                EntryKind::Mmio(sz) => entries.push(migrate::BarEntryV1 {
                    n: idx as u8,
                    kind: migrate::BarKindV1::Mmio,
                    size: sz as u64,
                    value: entry.value,
                }),
                EntryKind::Mmio64(sz) => entries.push(migrate::BarEntryV1 {
                    n: idx as u8,
                    kind: migrate::BarKindV1::Mmio64,
                    size: sz,
                    value: entry.value,
                }),
                EntryKind::Empty | EntryKind::Mmio64High => {}
            }
        }
        migrate::BarStateV1 { entries }
    }
}

pub mod migrate {
    use serde::Serialize;

    #[derive(Serialize)]
    pub enum BarKindV1 {
        Pio,
        Mmio,
        Mmio64,
    }
    #[derive(Serialize)]
    pub struct BarEntryV1 {
        pub n: u8,
        pub kind: BarKindV1,
        pub size: u64,
        pub value: u64,
    }
    #[derive(Serialize)]
    pub struct BarStateV1 {
        pub entries: Vec<BarEntryV1>,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::convert::TryFrom;

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
            bars.reg_write(BarN::try_from(i).unwrap(), 0xffffffff);
        }
        assert_eq!(bars.reg_read(BarN::BAR0), 0x0000ff01);
        assert_eq!(bars.reg_read(BarN::BAR1), 0xfffe0000);
        assert_eq!(bars.reg_read(BarN::BAR2), 0xfffc0004);
        assert_eq!(bars.reg_read(BarN::BAR3), 0xffffffff);
        assert_eq!(bars.reg_read(BarN::BAR4), 0x00000004);
        assert_eq!(bars.reg_read(BarN::BAR5), 0xfffffffe);
    }
}
