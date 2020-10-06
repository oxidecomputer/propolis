use std::marker::PhantomData;
use std::sync::{Arc, Mutex};

use super::bits::*;
use super::PciEndpoint;
use crate::types::*;
use crate::util::regmap::{Flags, RegMap};
use crate::util::self_arc::*;

use byteorder::{ByteOrder, LE};
use lazy_static::lazy_static;

#[derive(Copy, Clone, Eq, PartialEq, Debug)]
enum StdCfgReg {
    VendorId,
    DeviceId,
    Command,
    Status,
    RevisionId,
    ProgIf,
    Subclass,
    Class,
    CacheLineSize,
    LatencyTimer,
    HeaderType,
    Bist,
    Bar(BarN),
    CardbusPtr,
    SubVendorId,
    SubDeviceId,
    ExpansionRomAddr,
    CapPtr,
    Reserved,
    IntrLine,
    IntrPin,
    MinGrant,
    MaxLatency,
}

#[repr(u8)]
#[derive(Copy, Clone, Eq, PartialEq, Debug)]
pub enum BarN {
    BAR0 = 0,
    BAR1,
    BAR2,
    BAR3,
    BAR4,
    BAR5,
}

#[derive(Eq, PartialEq, Clone, Copy, Debug)]
enum BarDefine {
    Pio(u16),
    Mmio(u32),
    Mmio64(u64),
    Mmio64High,
}

lazy_static! {
    static ref STD_CFG_MAP: RegMap<StdCfgReg> = {
        let layout = [
            (StdCfgReg::VendorId, OFF_CFG_VENDORID, 2),
            (StdCfgReg::DeviceId, OFF_CFG_DEVICEID, 2),
            (StdCfgReg::Command, OFF_CFG_COMMAND, 2),
            (StdCfgReg::Status, OFF_CFG_STATUS, 2),
            (StdCfgReg::RevisionId, OFF_CFG_REVISIONID, 1),
            (StdCfgReg::ProgIf, OFF_CFG_PROGIF, 1),
            (StdCfgReg::Subclass, OFF_CFG_SUBCLASS, 1),
            (StdCfgReg::Class, OFF_CFG_CLASS, 1),
            (StdCfgReg::CacheLineSize, OFF_CFG_CACHELINESZ, 1),
            (StdCfgReg::LatencyTimer, OFF_CFG_LATENCYTIMER, 1),
            (StdCfgReg::HeaderType, OFF_CFG_HEADERTYPE, 1),
            (StdCfgReg::Bist, OFF_CFG_BIST, 1),
            (StdCfgReg::Bar(BarN::BAR0), OFF_CFG_BAR0, 4),
            (StdCfgReg::Bar(BarN::BAR1), OFF_CFG_BAR1, 4),
            (StdCfgReg::Bar(BarN::BAR2), OFF_CFG_BAR2, 4),
            (StdCfgReg::Bar(BarN::BAR3), OFF_CFG_BAR3, 4),
            (StdCfgReg::Bar(BarN::BAR4), OFF_CFG_BAR4, 4),
            (StdCfgReg::Bar(BarN::BAR5), OFF_CFG_BAR5, 4),
            (StdCfgReg::CardbusPtr, OFF_CFG_CARDBUSPTR, 4),
            (StdCfgReg::SubVendorId, OFF_CFG_SUBVENDORID, 2),
            (StdCfgReg::SubDeviceId, OFF_CFG_SUBDEVICEID, 2),
            (StdCfgReg::ExpansionRomAddr, OFF_CFG_EXPROMADDR, 4),
            (StdCfgReg::CapPtr, OFF_CFG_CAPPTR, 1),
            // Reserved bytes between CapPtr and IntrLine [0x35-0x3c)
            (
                StdCfgReg::Reserved,
                OFF_CFG_RESERVED,
                OFF_CFG_INTRLINE - OFF_CFG_RESERVED,
            ),
            (StdCfgReg::IntrLine, OFF_CFG_INTRLINE, 1),
            (StdCfgReg::IntrPin, OFF_CFG_INTRPIN, 1),
            (StdCfgReg::MinGrant, OFF_CFG_MINGRANT, 1),
            (StdCfgReg::MaxLatency, OFF_CFG_MAXLATENCY, 1),
        ];
        let mut map = RegMap::new(LEN_CFG_STD);
        for reg in layout.iter() {
            let (id, off, size) = (reg.0, reg.1 as usize, reg.2 as usize);
            let flags = match id {
                StdCfgReg::Reserved => {
                    // The reserved section is empty, so the register does not
                    // need a buffer padded to its own size for reads or writes.
                    Flags::NO_READ_EXTEND | Flags::NO_WRITE_EXTEND
                }
                _ => Flags::DEFAULT,
            };
            map.define_with_flags(off, size, id, flags);
        }
        map
    };
}

pub struct Ident {
    pub vendor_id: u16,
    pub device_id: u16,
    pub class: u8,
    pub subclass: u8,
    pub sub_vendor_id: u16,
    pub sub_device_id: u16,
}

struct Header {
    command: u16,
    intr_line: u8,
}

#[derive(Copy, Clone)]
struct BarEntry {
    define: Option<BarDefine>,
    addr: u64,
}
impl Default for BarEntry {
    fn default() -> Self {
        Self { define: None, addr: 0 }
    }
}

struct Bars {
    entries: [BarEntry; 6],
}

impl Bars {
    fn new() -> Self {
        Self { entries: [Default::default(); 6] }
    }
    fn read(&self, bar: BarN) -> u32 {
        let idx = bar as usize;
        let ent = &self.entries[idx];
        if ent.define.is_none() {
            return 0;
        }
        match ent.define.as_ref().unwrap() {
            BarDefine::Pio(_) => ent.addr as u32 | BAR_TYPE_IO,
            BarDefine::Mmio(_) => ent.addr as u32 | BAR_TYPE_MEM,
            BarDefine::Mmio64(_) => ent.addr as u32 | BAR_TYPE_MEM64,
            BarDefine::Mmio64High => {
                assert!(idx > 0);
                let prev = self.entries[idx - 1];
                (prev.addr >> 32) as u32
            }
        }
    }
    fn write(&mut self, bar: BarN, val: u32) {
        let idx = bar as usize;
        if self.entries[idx].define.is_none() {
            return;
        }
        let mut ent = &mut self.entries[idx];
        let old = match ent.define.as_ref().unwrap() {
            BarDefine::Pio(size) => {
                let mask = !(size - 1) as u32;
                let old = ent.addr;
                ent.addr = (val & mask) as u64;
                old
            }
            BarDefine::Mmio(size) => {
                let mask = !(size - 1);
                let old = ent.addr;
                ent.addr = (val & mask) as u64;
                old
            }
            BarDefine::Mmio64(size) => {
                let old = ent.addr;
                let mask = !(size - 1) as u32;
                let low = old as u32 & mask;
                ent.addr = (old & (0xffffffff << 32)) | low as u64;
                old
            }
            BarDefine::Mmio64High => {
                assert!(idx > 0);
                ent = &mut self.entries[idx - 1];
                let size = match ent.define.as_ref().unwrap() {
                    BarDefine::Mmio64(sz) => sz,
                    _ => panic!(),
                };
                let mask = !(size - 1);
                let old = ent.addr;
                ent.addr = ((val as u64) << 32) & mask | (old & 0xffffffff);
                old
            }
        };
        println!(
            "bar write {:x?} {:x} -> {:x}",
            *ent.define.as_ref().unwrap(),
            old,
            ent.addr
        );
    }
}

pub struct DeviceInst<I: Send> {
    ident: Ident,
    intr_pin: u8,

    header: Mutex<Header>,
    bars: Mutex<Bars>,

    sa_cell: SelfArcCell<Self>,

    inner: I,
}

impl<I: Send> DeviceInst<I> {
    fn new(ident: Ident, bars: Bars, i: I) -> Self {
        Self {
            sa_cell: SelfArcCell::new(),
            ident,
            intr_pin: 0x00,
            header: Mutex::new(Header { command: 0, intr_line: 0xff }),
            bars: Mutex::new(bars),
            inner: i,
        }
    }

    fn cfg_std_read(&self, id: &StdCfgReg, ro: &mut ReadOp) {
        assert!(ro.offset == 0 || *id == StdCfgReg::Reserved);

        let buf = &mut ro.buf;
        match id {
            StdCfgReg::VendorId => LE::write_u16(buf, self.ident.vendor_id),
            StdCfgReg::DeviceId => LE::write_u16(buf, self.ident.device_id),
            StdCfgReg::Class => buf[0] = self.ident.class,
            StdCfgReg::Subclass => buf[0] = self.ident.subclass,
            StdCfgReg::SubVendorId => {
                LE::write_u16(buf, self.ident.sub_vendor_id)
            }
            StdCfgReg::SubDeviceId => {
                LE::write_u16(buf, self.ident.sub_device_id)
            }

            StdCfgReg::Command => {
                LE::write_u16(buf, self.header.lock().unwrap().command)
            }
            StdCfgReg::IntrLine => {
                buf[0] = self.header.lock().unwrap().intr_line
            }
            StdCfgReg::IntrPin => buf[0] = self.intr_pin,
            StdCfgReg::Bar(bar) => {
                LE::write_u32(buf, self.bars.lock().unwrap().read(*bar))
            }
            StdCfgReg::ExpansionRomAddr => {
                // no rom for now
                LE::write_u32(buf, 0);
            }
            StdCfgReg::Reserved => {
                buf.iter_mut().for_each(|b| *b = 0);
            }
            _ => {
                println!("Unhandled read {:?}", id);
                buf.iter_mut().for_each(|b| *b = 0);
            }
        }
    }

    fn cfg_std_write(&self, id: &StdCfgReg, wo: &WriteOp) {
        assert!(wo.offset == 0 || *id == StdCfgReg::Reserved);

        let buf = wo.buf;
        match id {
            StdCfgReg::Command => {
                let val = LE::read_u16(buf);
                // XXX: wire up change handling
                self.header.lock().unwrap().command = val & REG_MASK_CMD;
            }
            StdCfgReg::IntrLine => {
                self.header.lock().unwrap().intr_line = buf[0];
            }
            StdCfgReg::Bar(bar) => {
                let val = LE::read_u32(buf);
                self.bars.lock().unwrap().write(*bar, val);
            }
            StdCfgReg::Reserved => {}
            _ => {
                println!("Unhandled write {:?}", id);
                // discard all other writes
            }
        }
    }
}

impl<I: Send + Sync> PciEndpoint for DeviceInst<I> {
    fn cfg_read(&self, ro: &mut ReadOp) {
        STD_CFG_MAP
            .with_ctx(self, Self::cfg_std_read, Self::cfg_std_write)
            .read(ro);
    }

    fn cfg_write(&self, wo: &WriteOp) {
        STD_CFG_MAP
            .with_ctx(self, Self::cfg_std_read, Self::cfg_std_write)
            .write(wo);
    }
}

impl<I: Sized + Send> SelfArc for DeviceInst<I> {
    fn self_arc_cell(&self) -> &SelfArcCell<Self> {
        &self.sa_cell
    }
}

pub trait Device: Send {
    fn bar_read(&self, bar: BarN, ro: &mut ReadOp) {
        panic!(
            "unexpected BAR read: {:?} @ {} {}",
            bar,
            ro.offset,
            ro.buf.len()
        );
    }
    fn bar_write(&self, bar: BarN, wo: &WriteOp) {
        panic!(
            "unexpected BAR write: {:?} @ {} {}",
            bar,
            wo.offset,
            wo.buf.len()
        );
    }
    // TODO
    // fn cap_read(&self);
    // fn cap_write(&self);
}

pub struct Builder<I> {
    ident: Ident,
    bars: [Option<BarDefine>; 6],
    _phantom: PhantomData<I>,
}

impl<I: Device> Builder<I> {
    pub fn new(ident: Ident) -> Self {
        Self { ident, bars: [None; 6], _phantom: PhantomData }
    }

    pub fn add_bar_io(mut self, bar: BarN, size: u16) -> Self {
        assert!(size.is_power_of_two());
        assert!(size >= 4);

        let idx = bar as usize;
        assert!(self.bars[idx].is_none());

        self.bars[idx] = Some(BarDefine::Pio(size));
        self
    }
    pub fn add_bar_mmio(mut self, bar: BarN, size: u32) -> Self {
        assert!(size.is_power_of_two());
        assert!(size >= 16);

        let idx = bar as usize;
        assert!(self.bars[idx].is_none());

        self.bars[idx] = Some(BarDefine::Mmio(size));
        self
    }
    pub fn add_bar_mmio64(mut self, bar: BarN, size: u64) -> Self {
        assert!(size.is_power_of_two());
        assert!(size >= 16);

        let idx = bar as usize;
        assert!(idx != 6);
        assert!(self.bars[idx].is_none());
        assert!(self.bars[idx + 1].is_none());

        self.bars[idx] = Some(BarDefine::Mmio64(size));
        self.bars[idx + 1] = Some(BarDefine::Mmio64High);
        self
    }

    fn generate_bars(&self) -> Bars {
        let mut bars = Bars::new();
        for (idx, ent) in self.bars.iter().enumerate() {
            bars.entries[idx].define = *ent;
        }
        bars
    }

    pub fn finish(self, inner: I) -> Arc<DeviceInst<I>> {
        let bars = self.generate_bars();
        let mut inst = Arc::new(DeviceInst::new(self.ident, bars, inner));
        SelfArc::self_arc_init(&mut inst);
        inst
    }
}
