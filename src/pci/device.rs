use std::convert::TryFrom;
use std::marker::PhantomData;
use std::sync::{Arc, Mutex};

use super::bits::*;
use super::{INTxPin, PciEndpoint};
use crate::common::*;
use crate::dispatch::DispCtx;
use crate::intr_pins::{IntrPin, IsaPin};
use crate::pio::PioDev;
use crate::util::regmap::{Flags, RegMap};
use crate::util::self_arc::*;

use byteorder::{ByteOrder, LE};
use lazy_static::lazy_static;
use num_enum::TryFromPrimitive;

enum CfgReg {
    Std,
    Custom,
}

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

#[derive(Copy, Clone, Eq, PartialEq, Debug, TryFromPrimitive)]
#[repr(u8)]
pub enum BarN {
    BAR0 = 0,
    BAR1,
    BAR2,
    BAR3,
    BAR4,
    BAR5,
}

#[derive(Eq, PartialEq, Clone, Copy, Debug)]
pub enum BarDefine {
    Pio(u16),
    Mmio(u32),
    Mmio64(u64),
    Mmio64High,
}

lazy_static! {
    static ref STD_CFG_MAP: RegMap<StdCfgReg> = {
        let layout = [
            (StdCfgReg::VendorId, 2),
            (StdCfgReg::DeviceId, 2),
            (StdCfgReg::Command, 2),
            (StdCfgReg::Status, 2),
            (StdCfgReg::RevisionId, 1),
            (StdCfgReg::ProgIf, 1),
            (StdCfgReg::Subclass, 1),
            (StdCfgReg::Class, 1),
            (StdCfgReg::CacheLineSize, 1),
            (StdCfgReg::LatencyTimer, 1),
            (StdCfgReg::HeaderType, 1),
            (StdCfgReg::Bist, 1),
            (StdCfgReg::Bar(BarN::BAR0), 4),
            (StdCfgReg::Bar(BarN::BAR1), 4),
            (StdCfgReg::Bar(BarN::BAR2), 4),
            (StdCfgReg::Bar(BarN::BAR3), 4),
            (StdCfgReg::Bar(BarN::BAR4), 4),
            (StdCfgReg::Bar(BarN::BAR5), 4),
            (StdCfgReg::CardbusPtr, 4),
            (StdCfgReg::SubVendorId, 2),
            (StdCfgReg::SubDeviceId, 2),
            (StdCfgReg::ExpansionRomAddr, 4),
            (StdCfgReg::CapPtr, 1),
            (StdCfgReg::Reserved, 7),
            (StdCfgReg::IntrLine, 1),
            (StdCfgReg::IntrPin, 1),
            (StdCfgReg::MinGrant, 1),
            (StdCfgReg::MaxLatency, 1),
        ];
        RegMap::create_packed(LEN_CFG_STD, &layout, Some(StdCfgReg::Reserved))
    };
}

#[derive(Default)]
pub struct Ident {
    pub vendor_id: u16,
    pub device_id: u16,
    pub class: u8,
    pub subclass: u8,
    pub prog_if: u8,
    pub revision_id: u8,
    pub sub_vendor_id: u16,
    pub sub_device_id: u16,
}

#[derive(Default)]
struct State {
    reg_command: RegCmd,
    reg_intr_line: u8,
    reg_intr_pin: u8,

    lintr_pin: Option<IsaPin>,
}

#[derive(Default)]
struct BarState {
    addr: u64,
    registered: bool,
}
struct BarEntry {
    define: Option<BarDefine>,
    state: Mutex<BarState>,
}
impl BarEntry {
    fn new() -> Self {
        Self { define: None, state: Mutex::new(Default::default()) }
    }
}

struct Bars {
    entries: [BarEntry; 6],
}

impl Bars {
    fn new() -> Self {
        Self {
            entries: [
                BarEntry::new(),
                BarEntry::new(),
                BarEntry::new(),
                BarEntry::new(),
                BarEntry::new(),
                BarEntry::new(),
            ],
        }
    }
    fn reg_read(&self, bar: BarN) -> u32 {
        let idx = bar as usize;
        let ent = &self.entries[idx];
        if ent.define.is_none() {
            return 0;
        }
        let state = ent.state.lock().unwrap();
        match ent.define.as_ref().unwrap() {
            BarDefine::Pio(_) => state.addr as u32 | BAR_TYPE_IO,
            BarDefine::Mmio(_) => state.addr as u32 | BAR_TYPE_MEM,
            BarDefine::Mmio64(_) => state.addr as u32 | BAR_TYPE_MEM64,
            BarDefine::Mmio64High => {
                assert_ne!(idx, 0);
                drop(state);
                let prev = self.entries[idx - 1].state.lock().unwrap();
                (prev.addr >> 32) as u32
            }
        }
    }
    fn reg_write<F>(&self, bar: BarN, val: u32, register: F)
    where
        F: Fn(&BarDefine, u64, u64) -> bool,
    {
        let idx = bar as usize;
        if self.entries[idx].define.is_none() {
            return;
        }
        let mut ent = &self.entries[idx];
        let mut state = self.entries[idx].state.lock().unwrap();
        let (old, mut state) = match ent.define.as_ref().unwrap() {
            BarDefine::Pio(size) => {
                let mask = !(size - 1) as u32;
                let old = state.addr;
                state.addr = (val & mask) as u64;
                (old, state)
            }
            BarDefine::Mmio(size) => {
                let mask = !(size - 1);
                let old = state.addr;
                state.addr = (val & mask) as u64;
                (old, state)
            }
            BarDefine::Mmio64(size) => {
                let old = state.addr;
                let mask = !(size - 1) as u32;
                let low = old as u32 & mask;
                state.addr = (old & (0xffffffff << 32)) | low as u64;
                (old, state)
            }
            BarDefine::Mmio64High => {
                assert!(idx > 0);
                drop(state);
                ent = &self.entries[idx - 1];
                let mut state = ent.state.lock().unwrap();
                let size = match ent.define.as_ref().unwrap() {
                    BarDefine::Mmio64(sz) => sz,
                    _ => panic!(),
                };
                let mask = !(size - 1);
                let old = state.addr;
                state.addr = ((val as u64) << 32) & mask | (old & 0xffffffff);
                (old, state)
            }
        };
        if state.registered && old != state.addr {
            // attempt to register BAR at new location
            state.registered =
                register(ent.define.as_ref().unwrap(), old, state.addr);
        }
        println!("bar write {:x?} {:x} -> {:x}", bar, old, state.addr);
    }
    fn change_registrations<F>(&self, changef: F)
    where
        F: Fn(BarN, &BarDefine, u64, bool) -> bool,
    {
        self.for_each(|barn, def| {
            let mut state = self.entries[barn as usize].state.lock().unwrap();
            state.registered = changef(barn, def, state.addr, state.registered);
        });
    }
    fn for_each<F>(&self, mut f: F)
    where
        F: FnMut(BarN, &BarDefine),
    {
        for (n, bar) in
            self.entries.iter().enumerate().filter(|(n, b)| b.define.is_some())
        {
            let barn = BarN::try_from(n as u8).unwrap();
            f(barn, bar.define.as_ref().unwrap());
        }
    }
    fn place(&self, bar: BarN, addr: u64) {
        let idx = bar as usize;
        assert!(self.entries[idx].define.is_some());

        let ent = &self.entries[idx].define.as_ref().unwrap();
        let mut state = self.entries[idx].state.lock().unwrap();
        match ent {
            BarDefine::Pio(_) => {
                assert!(addr <= u16::MAX as u64);
            }
            BarDefine::Mmio(_) => {
                assert!(addr <= u32::MAX as u64);
            }
            BarDefine::Mmio64(_) => {}
            BarDefine::Mmio64High => panic!(),
        }
        // initial BAR placement is a necessary step prior to registration
        assert!(!state.registered);
        state.addr = addr;
    }
}

pub struct DeviceInst<I: Send + 'static> {
    ident: Ident,
    lintr_req: bool,
    cfg_space: RegMap<CfgReg>,

    state: Mutex<State>,
    bars: Bars,

    sa_cell: SelfArcCell<Self>,

    inner: I,
}

impl<I: Device> DeviceInst<I> {
    fn new(ident: Ident, cfg_space: RegMap<CfgReg>, bars: Bars, i: I) -> Self {
        Self {
            ident,
            lintr_req: false,
            cfg_space,
            state: Mutex::new(State {
                reg_intr_line: 0xff,
                ..Default::default()
            }),
            bars,
            sa_cell: SelfArcCell::new(),
            inner: i,
        }
    }
    pub fn with_inner<F, T>(&self, f: F) -> T
    where
        F: FnOnce(&I) -> T,
    {
        f(&self.inner)
    }

    fn cfg_std_read(&self, id: &StdCfgReg, ro: &mut ReadOp, ctx: &DispCtx) {
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
            StdCfgReg::ProgIf => buf[0] = self.ident.prog_if,
            StdCfgReg::RevisionId => buf[0] = self.ident.revision_id,

            StdCfgReg::Command => {
                let val = self.state.lock().unwrap().reg_command.bits();
                LE::write_u16(buf, val);
            }
            StdCfgReg::Status => {
                let mut val = RegStatus::empty();
                if self.lintr_req {
                    let state = self.state.lock().unwrap();
                    if let Some(pin) = state.lintr_pin.as_ref() {
                        if pin.is_asserted() {
                            val.insert(RegStatus::INTR_STATUS);
                        }
                    }
                }
                LE::write_u16(buf, val.bits());
            }
            StdCfgReg::IntrLine => {
                buf[0] = self.state.lock().unwrap().reg_intr_line
            }
            StdCfgReg::IntrPin => {
                buf[0] = self.state.lock().unwrap().reg_intr_pin
            }
            StdCfgReg::Bar(bar) => LE::write_u32(buf, self.bars.reg_read(*bar)),
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
    fn cfg_std_write(&self, id: &StdCfgReg, wo: &WriteOp, ctx: &DispCtx) {
        assert!(wo.offset == 0 || *id == StdCfgReg::Reserved);

        let buf = wo.buf;
        match id {
            StdCfgReg::Command => {
                let new = RegCmd::from_bits_truncate(LE::read_u16(buf));
                self.reg_cmd_write(new, ctx);
            }
            StdCfgReg::IntrLine => {
                self.state.lock().unwrap().reg_intr_line = buf[0];
            }
            StdCfgReg::Bar(bar) => {
                let val = LE::read_u32(buf);
                let state = self.state.lock().unwrap();
                self.bars.reg_write(*bar, val, |def, old, new| {
                    // fail move for now
                    match def {
                        BarDefine::Pio(sz) => {
                            if !state.reg_command.contains(RegCmd::IO_EN) {
                                // pio mappings are disabled via cmd reg
                                return false;
                            }
                            ctx.mctx.with_pio(|bus| {
                                // We know this was previously registered
                                let (dev, old_bar) =
                                    bus.unregister(old as u16).unwrap();
                                assert_eq!(old_bar, *bar as usize);
                                bus.register(
                                    new as u16,
                                    *sz,
                                    dev,
                                    *bar as usize,
                                )
                                .is_err()
                            })
                        }
                        _ => {
                            if !state.reg_command.contains(RegCmd::IO_EN) {
                                // pio mappings are disabled via cmd reg
                                return false;
                            }
                            todo!("wire up MMIO later")
                        }
                    }
                });
            }
            StdCfgReg::VendorId
            | StdCfgReg::DeviceId
            | StdCfgReg::Class
            | StdCfgReg::Subclass
            | StdCfgReg::SubVendorId
            | StdCfgReg::SubDeviceId
            | StdCfgReg::ProgIf
            | StdCfgReg::RevisionId
            | StdCfgReg::Reserved => {
                // ignore writes to RO fields
            }
            _ => {
                println!("Unhandled write {:?}", id);
                // discard all other writes
            }
        }
    }
    fn reg_cmd_write(&self, val: RegCmd, ctx: &DispCtx) {
        let mut state = self.state.lock().unwrap();
        let diff = val ^ state.reg_command;
        if diff.intersects(RegCmd::IO_EN | RegCmd::MMIO_EN) {
            // change bar mapping state
            self.bars.change_registrations(|bar, def, addr, registered| {
                match def {
                    BarDefine::Pio(sz) => {
                        if registered && !val.contains(RegCmd::IO_EN) {
                            // unregister
                            ctx.mctx.with_pio(|bus| {
                                bus.unregister(addr as u16).unwrap();
                            });
                            false
                        } else if !registered && val.contains(RegCmd::IO_EN) {
                            // register
                            ctx.mctx.with_pio(|bus| {
                                bus.register(
                                    addr as u16,
                                    *sz as u16,
                                    self.self_weak(),
                                    bar as usize,
                                )
                                .is_ok()
                            })
                        } else {
                            registered
                        }
                    }
                    _ => todo!("wire up MMIO later"),
                }
            });
        }
        if diff.intersects(RegCmd::INTX_DIS) {
            // toggle lintr state
        }
        state.reg_command = val;
    }
}

impl<I: Device> PciEndpoint for DeviceInst<I> {
    fn cfg_rw(&self, rwo: &mut RWOp, ctx: &DispCtx) {
        self.cfg_space.process(rwo, |id, rwo| match id {
            CfgReg::Std => {
                STD_CFG_MAP.process(rwo, |id, rwo| match rwo {
                    RWOp::Read(ro) => self.cfg_std_read(id, ro, ctx),
                    RWOp::Write(wo) => self.cfg_std_write(id, wo, ctx),
                });
            }
            CfgReg::Custom => match rwo {
                RWOp::Read(ro) => self.inner.cfg_read(ro),
                RWOp::Write(wo) => self.inner.cfg_write(wo),
            },
        });
    }
    fn attach(&self, get_lintr: &dyn Fn() -> (INTxPin, IsaPin)) {
        let mut state = self.state.lock().unwrap();
        if self.lintr_req {
            let (intx, isa_pin) = get_lintr();
            state.reg_intr_pin = intx as u8;
            state.reg_intr_line = isa_pin.get_pin();
            state.lintr_pin = Some(isa_pin);
        }
    }

    fn place_bars(
        &self,
        place_bar: &mut dyn FnMut(BarN, &BarDefine) -> u64,
        ctx: &DispCtx,
    ) {
        self.bars.for_each(|bar, def| {
            let addr = place_bar(bar, def);
            self.bars.place(bar, addr);
        });
        let state = self.state.lock().unwrap();
        if state.reg_command.intersects(RegCmd::IO_EN | RegCmd::MMIO_EN) {
            self.bars.change_registrations(|bar, def, addr, registered| {
                assert!(!registered);
                match def {
                    BarDefine::Pio(sz) => {
                        if state.reg_command.intersects(RegCmd::IO_EN) {
                            ctx.mctx.with_pio(|bus| {
                                bus.register(
                                    addr as u16,
                                    *sz as u16,
                                    self.self_weak(),
                                    bar as usize,
                                )
                                .is_ok()
                            })
                        } else {
                            false
                        }
                    }
                    _ => todo!("wire up MMIO later"),
                }
            });
        }
    }
}

impl<I: Device> PioDev for DeviceInst<I> {
    fn pio_in(&self, port: u16, ident: usize, ro: &mut ReadOp, ctx: &DispCtx) {
        let bar = BarN::try_from(ident as u8).unwrap();
        self.inner.bar_rw(bar, &mut RWOp::Read(ro), ctx);
    }

    fn pio_out(&self, port: u16, ident: usize, wo: &WriteOp, ctx: &DispCtx) {
        let bar = BarN::try_from(ident as u8).unwrap();
        self.inner.bar_rw(bar, &mut RWOp::Write(wo), ctx);
    }
}

impl<I: Sized + Send> SelfArc for DeviceInst<I> {
    fn self_arc_cell(&self) -> &SelfArcCell<Self> {
        &self.sa_cell
    }
}

pub struct DeviceCtx<'a, 'b> {
    state: &'a Mutex<State>,
    dctx: &'b DispCtx,
}
impl<'a, 'b> DeviceCtx<'a, 'b> {
    fn new(state: &'a Mutex<State>, dctx: &'b DispCtx) -> Self {
        Self { state, dctx }
    }

    pub fn set_lintr(&self, level: bool) {
        let mut state = self.state.lock().unwrap();
        if state.reg_intr_pin == 0 {
            return;
        }
        // XXX: heed INTxDIS
        let pin = state.lintr_pin.as_mut().unwrap();
        if level {
            pin.assert();
        } else {
            pin.deassert();
        }
    }
}

pub trait Device: Send + Sync {
    fn bar_rw(&self, bar: BarN, rwo: &mut RWOp, ctx: &DispCtx) {
        match rwo {
            RWOp::Read(ro) => {
                unimplemented!("BAR read ({:?} @ {:x})", bar, ro.offset)
            }
            RWOp::Write(wo) => {
                unimplemented!("BAR write ({:?} @ {:x})", bar, wo.offset)
            }
        }
    }

    fn cfg_read(&self, ro: &mut ReadOp) {
        unimplemented!("CFG read @ {:x}", ro.offset)
    }
    fn cfg_write(&self, wo: &WriteOp) {
        unimplemented!("CFG write @ {:x}", wo.offset)
    }
    // TODO
    // fn cap_read(&self);
    // fn cap_write(&self);
}

pub struct Builder<I> {
    ident: Ident,
    lintr_req: bool,
    bars: [Option<BarDefine>; 6],
    cfgmap: RegMap<CfgReg>,
    _phantom: PhantomData<I>,
}

impl<I: Device> Builder<I> {
    pub fn new(ident: Ident) -> Self {
        let mut cfgmap = RegMap::new(LEN_CFG);
        cfgmap.define_with_flags(0, LEN_CFG_STD, CfgReg::Std, Flags::PASSTHRU);
        Self {
            ident,
            lintr_req: false,
            bars: [None; 6],
            cfgmap,
            _phantom: PhantomData,
        }
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
    pub fn add_lintr(mut self) -> Self {
        self.lintr_req = true;
        self
    }
    pub fn add_custom_cfg(mut self, offset: u8, len: u8) -> Self {
        self.cfgmap.define_with_flags(
            offset as usize,
            len as usize,
            CfgReg::Custom,
            Flags::PASSTHRU,
        );
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

        let mut inst = DeviceInst::new(self.ident, self.cfgmap, bars, inner);
        inst.lintr_req = self.lintr_req;

        let mut done = Arc::new(inst);
        SelfArc::self_arc_init(&mut done);
        done
    }
}
