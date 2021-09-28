use std::fmt::Result as FmtResult;
use std::fmt::{Display, Formatter};
use std::io::{Error, ErrorKind};
use std::str::FromStr;
use std::sync::{Arc, Mutex};

use crate::common::*;
use crate::dispatch::DispCtx;
use crate::intr_pins::IntrPin;

pub mod bits;
mod device;

pub use device::*;

pub const PORT_PCI_CONFIG_ADDR: u16 = 0xcf8;
pub const PORT_PCI_CONFIG_DATA: u16 = 0xcfc;

const MASK_FUNC: u8 = 0x07;
const MASK_DEV: u8 = 0x1f;
const MASK_BUS: u8 = 0xff;

/// Bus, Device, Function.
///
/// Acts as an address for PCI and PCIe device functionality.
#[derive(Copy, Clone, Eq, PartialEq, Debug)]
pub struct Bdf {
    inner_bus: u8,
    inner_dev: u8,
    inner_func: u8,
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

        Bdf::try_new(fields[0], fields[1], fields[2]).ok_or_else(|| {
            Error::new(
                ErrorKind::InvalidInput,
                "Failed to parse as BDF".to_string(),
            )
        })
    }
}

impl Bdf {
    /// Creates a new Bdf.
    ///
    /// The bus/device/function values must be within the acceptable range for
    /// PCI addressing. If they could be invalid, `Bdf::try_new` should be used
    /// instead.
    ///
    /// # Panics
    ///
    /// - Panics if `dev` is larger than 0x1F.
    /// - Panics if `func` is larger than 0x07.
    pub fn new(bus: u8, dev: u8, func: u8) -> Self {
        assert!(dev <= MASK_DEV);
        assert!(func <= MASK_FUNC);

        Self { inner_bus: bus, inner_dev: dev, inner_func: func }
    }

    /// Attempts to make a new BDF.
    ///
    /// Returns [`Option::None`] if the values would not fit within a BDF.
    pub fn try_new(bus: u8, dev: u8, func: u8) -> Option<Self> {
        if dev <= MASK_DEV && func <= MASK_FUNC {
            Some(Self::new(bus, dev, func))
        } else {
            None
        }
    }
    pub fn bus(&self) -> u8 {
        self.inner_bus
    }
    pub fn dev(&self) -> u8 {
        self.inner_dev
    }
    pub fn func(&self) -> u8 {
        self.inner_func
    }
}
impl Display for Bdf {
    fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
        write!(f, "{}.{}.{}", self.inner_bus, self.inner_dev, self.inner_func)
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

pub trait Endpoint: Send + Sync {
    fn cfg_rw(&self, op: RWOp<'_, '_>, ctx: &DispCtx);
    fn attach(&self, get_lintr: &dyn Fn() -> (INTxPinID, Arc<dyn IntrPin>));
    fn bar_for_each(&self, cb: &mut dyn FnMut(BarN, &BarDefine));
    fn bar_place(&self, bar: BarN, addr: u64);
    fn as_devinst(&self) -> Option<&DeviceInst>;
}

const SLOTS_PER_BUS: usize = 32;
const FUNCS_PER_SLOT: usize = 8;

#[derive(Default)]
pub struct Slot {
    funcs: [Option<Arc<dyn Endpoint>>; FUNCS_PER_SLOT],
}

#[derive(Default)]
pub struct Bus {
    slots: [Slot; SLOTS_PER_BUS],
}

impl Bus {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn attach(&mut self, slot: u8, func: u8, dev: Arc<dyn Endpoint>) {
        assert!((slot as usize) < SLOTS_PER_BUS);
        assert!((func as usize) < FUNCS_PER_SLOT);

        // XXX be strict for now
        assert!(self.slots[slot as usize].funcs[func as usize].is_none());
        self.slots[slot as usize].funcs[func as usize] = Some(dev);
    }

    pub fn iter(&self) -> Iter {
        Iter::new(self)
    }

    pub fn device_at(&self, slot: u8, func: u8) -> Option<&Arc<dyn Endpoint>> {
        assert!((slot as usize) < SLOTS_PER_BUS);
        assert!((func as usize) < FUNCS_PER_SLOT);

        self.slots[slot as usize].funcs[func as usize].as_ref()
    }
}

pub struct Iter<'a> {
    bus: &'a Bus,
    pos: usize,
}
impl<'a> Iter<'a> {
    fn new(bus: &'a Bus) -> Self {
        Self { bus, pos: 0 }
    }
    fn slot_func(&self) -> Option<(usize, usize)> {
        if self.pos < (SLOTS_PER_BUS * FUNCS_PER_SLOT) as usize {
            Some((self.pos / FUNCS_PER_SLOT, self.pos & MASK_FUNC as usize))
        } else {
            None
        }
    }
}
impl<'a> Iterator for Iter<'a> {
    type Item = (u8, u8, &'a Arc<dyn Endpoint>);

    fn next(&mut self) -> Option<Self::Item> {
        while let Some((slot, func)) = self.slot_func() {
            self.pos += 1;
            if self.bus.slots[slot].funcs[func].is_some() {
                return Some((
                    slot as u8,
                    func as u8,
                    self.bus.slots[slot].funcs[func].as_ref().unwrap(),
                ));
            }
        }
        None
    }
}

fn cfg_addr_parse(addr: u32) -> Option<(Bdf, u8)> {
    if addr & 0x80000000 == 0 {
        // Enable bit not set
        None
    } else {
        let offset = addr & 0xff;
        let func = (addr >> 8) as u8 & MASK_FUNC;
        let device = (addr >> 11) as u8 & MASK_DEV;
        let bus = (addr >> 16) as u8 & MASK_BUS;

        Some((Bdf::new(bus, device, func), offset as u8))
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
}
