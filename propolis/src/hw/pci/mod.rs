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
pub mod bridge;
pub mod bus;
mod cfgspace;
mod device;
pub mod topology;

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

/// A device/function located on a specific PCI bus.
#[derive(Copy, Clone, Eq, PartialEq, Debug, Ord, PartialOrd)]
pub struct BusLocation {
    pub dev: DevNum,
    pub func: FuncNum,
}

impl BusLocation {
    pub const fn new(dev: u8, func: u8) -> Option<Self> {
        let dnum = DevNum::new(dev);
        let fnum = FuncNum::new(func);
        match (dnum, fnum) {
            (Some(d), Some(f)) => Some(Self { dev: d, func: f }),
            _ => None,
        }
    }
}

/// Bus, Device, Function.
///
/// Acts as an address for PCI and PCIe device functionality.
#[derive(Copy, Clone, Eq, PartialEq, Debug, Ord, PartialOrd)]
pub struct Bdf {
    pub bus: BusNum,
    pub location: BusLocation,
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

impl TryFrom<propolis_types::PciPath> for Bdf {
    type Error = std::io::Error;

    fn try_from(value: propolis_types::PciPath) -> Result<Self, Self::Error> {
        Bdf::new(value.bus(), value.device(), value.function()).ok_or_else(|| {
            Error::new(
                ErrorKind::InvalidInput,
                "Failed to convert raw PCI path to BDF".to_string(),
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
        let loc = BusLocation::new(dev, func);
        match (bnum, loc) {
            (Some(b), Some(l)) => Some(Self { bus: b, location: l }),
            _ => None,
        }
    }
}
impl Display for Bdf {
    fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
        write!(
            f,
            "{}.{}.{}",
            self.bus.0, self.location.dev.0, self.location.func.0
        )
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

    pub(super) fn set_addr(&self, addr: u32) {
        let mut inner = self.addr.lock().unwrap();
        *inner = addr;
    }
}

pub struct PcieCfgDecoder {
    bus_mask: u8,
}

impl PcieCfgDecoder {
    /// Creates a PCIe config space access decoder that can address the supplied
    /// number of buses.
    ///
    /// The supplied bus count must be a power of 2 between
    /// [`bits::PCIE_MIN_BUSES_PER_ECAM_REGION`] and
    /// [`bits::PCIE_MAX_BUSES_PER_ECAM_REGION`] inclusive.
    pub fn new(bus_count: u16) -> Self {
        assert!(bus_count.is_power_of_two());
        assert!(bus_count >= bits::PCIE_MIN_BUSES_PER_ECAM_REGION);
        assert!(bus_count <= bits::PCIE_MAX_BUSES_PER_ECAM_REGION);

        Self { bus_mask: (bus_count - 1) as u8 }
    }

    /// Decodes a request to access PCIe configuration space and dispatches the
    /// resulting BDF and device-relative configuration space offset to a
    /// caller-supplied completion function.
    pub fn service<F>(&self, rwop: RWOp, mut cb: F)
    where
        F: FnMut(&Bdf, RWOp) -> Option<()>,
    {
        assert_ne!(rwop.len(), 0);
        let (bdf, cfg_off) = self.decode_enhanced_cfg_offset(rwop.offset());

        // Ensure the access is addressed to a single device.
        let (end_bdf, _) =
            self.decode_enhanced_cfg_offset(rwop.offset() + rwop.len() - 1);
        if bdf != end_bdf {
            if let RWOp::Read(ro) = rwop {
                ro.fill(0xff);
            }
            return;
        }
        match rwop {
            RWOp::Read(ro) => {
                let mut cro = ReadOp::new_child(cfg_off, ro, ..);
                let hit = cb(&bdf, RWOp::Read(&mut cro));
                if hit.is_none() {
                    cro.fill(0xff);
                }
            }
            RWOp::Write(wo) => {
                let mut cwo = WriteOp::new_child(cfg_off, wo, ..);
                let _ = cb(&bdf, RWOp::Write(&mut cwo));
            }
        }
    }

    /// Decodes an offset into a PCIe ECAM region into a bus/device/function and
    /// an offset into that function's configuration space.
    fn decode_enhanced_cfg_offset(&self, region_offset: usize) -> (Bdf, usize) {
        let bus = (region_offset >> 20) as u8 & self.bus_mask;
        let dev = (region_offset >> 15) as u8 & bits::MASK_DEV;
        let func = (region_offset >> 12) as u8 & bits::MASK_FUNC;
        let cfg_offset = region_offset & bits::MASK_ECAM_CFG_OFFSET;
        (Bdf::new(bus, dev, func).unwrap(), cfg_offset)
    }
}

pub mod migrate {
    pub use super::device::migrate::*;
}

#[cfg(test)]
mod test {
    use crate::common::{RWOp, ReadOp, WriteOp};

    use super::{bits, Bdf, PcieCfgDecoder};

    #[test]
    fn pcie_decoder() {
        let pcie = PcieCfgDecoder::new(bits::PCIE_MAX_BUSES_PER_ECAM_REGION);
        let mut buf = [0u8; 4];
        let mut ro = ReadOp::from_buf(0, &mut buf);
        pcie.service(RWOp::Read(&mut ro), |bdf, rwo| {
            assert_eq!(*bdf, Bdf::new(0, 0, 0).unwrap());
            assert!(matches!(rwo, RWOp::Read(_)));
            assert_eq!(rwo.offset(), 0);
            assert_eq!(rwo.len(), 4);
            Some(())
        });

        let buf = [0u8; 16];
        let mut wo = WriteOp::from_buf(0x400, &buf);
        pcie.service(RWOp::Write(&mut wo), |bdf, rwo| {
            assert_eq!(*bdf, Bdf::new(0, 0, 0).unwrap());
            assert!(matches!(rwo, RWOp::Write(_)));
            assert_eq!(rwo.offset(), 0x400);
            assert_eq!(rwo.len(), 16);
            Some(())
        })
    }

    #[test]
    fn pcie_decoder_multiple_bdfs() {
        let pcie = PcieCfgDecoder::new(bits::PCIE_MAX_BUSES_PER_ECAM_REGION);
        let mut buf = [0u8; 4];
        let mut ro = ReadOp::from_buf(1_usize << 12, &mut buf);
        pcie.service(RWOp::Read(&mut ro), |bdf, rwo| {
            assert_eq!(*bdf, Bdf::new(0, 0, 1).unwrap());
            assert_eq!(rwo.offset(), 0);
            Some(())
        });

        let mut ro =
            ReadOp::from_buf(4_usize << 15 | 3_usize << 12 | 0x123, &mut buf);
        pcie.service(RWOp::Read(&mut ro), |bdf, rwo| {
            assert_eq!(*bdf, Bdf::new(0, 4, 3).unwrap());
            assert_eq!(rwo.offset(), 0x123);
            Some(())
        });

        let mut ro = ReadOp::from_buf(
            133_usize << 20 | 7_usize << 15 | 1_usize << 12 | 0x337,
            &mut buf,
        );
        pcie.service(RWOp::Read(&mut ro), |bdf, rwo| {
            assert_eq!(*bdf, Bdf::new(133, 7, 1).unwrap());
            assert_eq!(rwo.offset(), 0x337);
            Some(())
        });
    }

    #[test]
    fn pcie_decoder_min_buses() {
        let pcie = PcieCfgDecoder::new(4);
        let mut buf = [0u8; 4];
        for seg_group in 0..4 {
            for bus in 0..4 {
                let mut ro =
                    ReadOp::from_buf((seg_group * 4 + bus) << 20, &mut buf);
                pcie.service(RWOp::Read(&mut ro), |bdf, rwo| {
                    assert_eq!(
                        *bdf,
                        Bdf::new(bus as u8, 0, 0).unwrap(),
                        "group {}, bus {}",
                        seg_group,
                        bus
                    );
                    assert_eq!(rwo.offset(), 0);
                    Some(())
                });
            }
        }
    }

    #[test]
    fn pcie_decoder_access_spans_multiple_devs() {
        let pcie = PcieCfgDecoder::new(bits::PCIE_MAX_BUSES_PER_ECAM_REGION);
        let mut buf = [0u8; 8];
        let mut ro = ReadOp::from_buf(0xffc, &mut buf);

        // This access spans multiple functions, so the decoder can't
        // meaningfully address a single BDF and should therefore not
        // invoke the closure.
        pcie.service(RWOp::Read(&mut ro), |_bdf, _rwo| panic!());
        assert_eq!(buf, [0xffu8; 8]);
    }
}
