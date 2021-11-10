use std::convert::TryFrom;
use std::fmt::Result as FmtResult;
use std::fmt::{Display, Formatter};
use std::io::{Error, ErrorKind};
use std::str::FromStr;
use std::sync::{Arc, Mutex};

use crate::common::*;
use crate::dispatch::DispCtx;
use crate::intr_pins::IntrPin;

use num_enum::TryFromPrimitive;

pub mod bar;
pub mod bits;
pub mod bus;
mod device;

pub use bus::Bus;
pub use device::*;

#[derive(Copy, Clone, Eq, PartialEq, Debug, Ord, PartialOrd)]
pub struct BusNum(u8);
impl BusNum {
    pub const fn new(n: u8) -> Option<Self> {
        Some(Self(n))
    }
    pub const fn get(&self) -> u8 {
        self.0
    }
}
#[derive(Copy, Clone, Eq, PartialEq, Debug, Ord, PartialOrd)]
pub struct DevNum(u8);
impl DevNum {
    pub const fn new(n: u8) -> Option<Self> {
        if n <= bits::MASK_DEV {
            Some(Self(n))
        } else {
            None
        }
    }
    pub const fn get(&self) -> u8 {
        self.0
    }
}
#[derive(Copy, Clone, Eq, PartialEq, Debug, Ord, PartialOrd)]
pub struct FuncNum(u8);
impl FuncNum {
    pub const fn new(n: u8) -> Option<Self> {
        if n <= bits::MASK_FUNC {
            Some(Self(n))
        } else {
            None
        }
    }
    pub const fn get(&self) -> u8 {
        self.0
    }
}

/// Bus, Device, Function.
///
/// Acts as an address for PCI and PCIe device functionality.
#[derive(Copy, Clone, Eq, PartialEq, Debug, Ord, PartialOrd)]
pub struct Bdf {
    pub bus: BusNum,
    pub dev: DevNum,
    pub func: FuncNum,
}

impl FromStr for Bdf {
    type Err = std::io::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut fields = Vec::with_capacity(3);
        for f in s.split('.') {
            let num = usize::from_str(f).map_err(|e| {
                Error::new(ErrorKind::InvalidInput, e.to_string())
            })?;
            if num > u8::MAX as usize {
                return Err(Error::new(
                    ErrorKind::InvalidInput,
                    format!("Value too large: {}", num),
                ));
            }
            fields.push(num as u8);
        }

        if fields.len() != 3 {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "Wrong number of fields for BDF",
            ));
        }

        Bdf::new(fields[0], fields[1], fields[2]).ok_or_else(|| {
            Error::new(
                ErrorKind::InvalidInput,
                "Failed to parse as BDF".to_string(),
            )
        })
    }
}

impl Bdf {
    /// Attempts to make a new BDF.
    ///
    /// Returns [`Option::None`] if the values would not fit within a BDF.
    pub const fn new(bus: u8, dev: u8, func: u8) -> Option<Self> {
        // Until the `?` operator is supported in `const fn`s, this more verbose
        // implementation is required.
        let bnum = BusNum::new(bus);
        let dnum = DevNum::new(dev);
        let fnum = FuncNum::new(func);
        match (bnum, dnum, fnum) {
            (Some(b), Some(d), Some(f)) => {
                Some(Self { bus: b, dev: d, func: f })
            }
            _ => None,
        }
    }
}
impl Display for Bdf {
    fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
        write!(f, "{}.{}.{}", self.bus.0, self.dev.0, self.func.0)
    }
}

#[derive(
    Copy, Clone, Eq, PartialEq, Debug, Ord, PartialOrd, TryFromPrimitive,
)]
#[repr(u8)]
pub enum BarN {
    BAR0 = 0,
    BAR1,
    BAR2,
    BAR3,
    BAR4,
    BAR5,
}
impl BarN {
    fn iter() -> BarIter {
        BarIter { n: 0 }
    }
}
struct BarIter {
    n: u8,
}
impl Iterator for BarIter {
    type Item = BarN;

    fn next(&mut self) -> Option<Self::Item> {
        let res = BarN::try_from(self.n).ok()?;
        self.n += 1;
        Some(res)
    }
}

#[repr(u8)]
#[derive(Copy, Clone)]
pub enum INTxPinID {
    IntA = 1,
    IntB = 2,
    IntC = 3,
    IntD = 4,
}

pub type LintrCfg = (INTxPinID, Arc<dyn IntrPin>);

pub trait Endpoint: Send + Sync {
    fn attach(&self, attachment: bus::Attachment);
    fn cfg_rw(&self, op: RWOp<'_, '_>, ctx: &DispCtx);
    fn bar_rw(&self, bar: BarN, rwo: RWOp, ctx: &DispCtx);
}

fn cfg_addr_parse(addr: u32) -> Option<(Bdf, u8)> {
    if addr & 0x80000000 == 0 {
        // Enable bit not set
        None
    } else {
        Some((
            Bdf::new(
                (addr >> 16) as u8 & bits::MASK_BUS,
                (addr >> 11) as u8 & bits::MASK_DEV,
                (addr >> 8) as u8 & bits::MASK_FUNC,
            )
            .unwrap(),
            (addr & 0xff) as u8,
        ))
    }
}

pub struct PioCfgDecoder {
    addr: Mutex<u32>,
}
impl PioCfgDecoder {
    pub fn new() -> Self {
        Self { addr: Mutex::new(0) }
    }
    pub fn service_addr(&self, rwop: RWOp) {
        if rwop.len() != 4 || rwop.offset() != 0 {
            // XXX expect aligned/sized reads
            return;
        }
        let mut addr = self.addr.lock().unwrap();
        match rwop {
            RWOp::Read(ro) => ro.write_u32(*addr),
            RWOp::Write(wo) => *addr = wo.read_u32(),
        }
    }
    pub fn service_data<F>(&self, rwop: RWOp, mut cb: F)
    where
        F: FnMut(&Bdf, RWOp) -> Option<()>,
    {
        let locked_addr = self.addr.lock().unwrap();
        let addr = *locked_addr;
        drop(locked_addr);

        if let Some((bdf, cfg_off)) = cfg_addr_parse(addr) {
            let off = cfg_off as usize + rwop.offset();
            match rwop {
                RWOp::Read(ro) => {
                    let mut cro = ReadOp::new_child(off, ro, ..);
                    let hit = cb(&bdf, RWOp::Read(&mut cro));
                    if hit.is_none() {
                        cro.fill(0xff);
                    }
                }
                RWOp::Write(wo) => {
                    let mut cwo = WriteOp::new_child(off, wo, ..);
                    let _ = cb(&bdf, RWOp::Write(&mut cwo));
                }
            };
        }
    }
    pub fn addr(&self) -> u32 {
        let addr = self.addr.lock().unwrap();
        *addr
    }
}

pub mod migrate {
    pub use super::device::migrate::*;
}
