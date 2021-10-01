use std::any::Any;
use std::convert::TryFrom;
use std::marker::PhantomData;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, Weak};

use super::bits::*;
use super::{Endpoint, INTxPinID};
use crate::common::*;
use crate::dispatch::DispCtx;
use crate::instance;
use crate::intr_pins::IntrPin;
use crate::inventory::Entity;
use crate::mmio::MmioDev;
use crate::pio::PioDev;
use crate::util::regmap::{Flags, RegMap};
use crate::util::self_arc::*;

use lazy_static::lazy_static;
use num_enum::TryFromPrimitive;

enum CfgReg {
    Std,
    Custom(u8),
    CapId(u8),
    CapNext(u8),
    CapBody(u8),
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

    lintr_pin: Option<Arc<dyn IntrPin>>,

    update_in_progress: bool,
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
    fn new(defs: &[Option<BarDefine>; 6]) -> Self {
        let mut this = Self {
            entries: [
                BarEntry::new(),
                BarEntry::new(),
                BarEntry::new(),
                BarEntry::new(),
                BarEntry::new(),
                BarEntry::new(),
            ],
        };
        for (idx, def) in
            defs.iter().enumerate().filter_map(|(n, def)| def.map(|d| (n, d)))
        {
            // Make sure 64-bit BAR definitions are playing by the rules
            if matches!(def, BarDefine::Mmio64(_)) {
                assert!(idx < 5);
                assert!(matches!(defs[idx + 1], Some(BarDefine::Mmio64High)));
            }
            if matches!(def, BarDefine::Mmio64High) {
                assert_ne!(idx, 0);
                assert!(matches!(defs[idx - 1], Some(BarDefine::Mmio64(_))));
            }
            this.entries[idx].define = Some(def);
        }

        this
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
                let low = val as u32 & mask;
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
                let high = (((val as u64) << 32) & mask) & 0xffffffff00000000;
                state.addr = high | (old & 0xffffffff);
                (old, state)
            }
        };
        if state.registered && old != state.addr {
            // attempt to register BAR at new location
            state.registered =
                register(ent.define.as_ref().unwrap(), old, state.addr);
        }
    }
    fn change_registrations<F>(&self, changef: F)
    where
        F: Fn(BarN, &BarDefine, u64, bool) -> Option<bool>,
    {
        self.for_each(|barn, def| {
            if def == &BarDefine::Mmio64High {
                // The high portion of 64-bit BARs does not require direct
                // handling, as the low portion bears the necessary information.
                return;
            }
            let mut state = self.entries[barn as usize].state.lock().unwrap();
            if let Some(new_reg_state) =
                changef(barn, def, state.addr, state.registered)
            {
                state.registered = new_reg_state;
            }
        });
    }
    fn for_each<F>(&self, mut f: F)
    where
        F: FnMut(BarN, &BarDefine),
    {
        for (n, bar) in
            self.entries.iter().enumerate().filter(|(_n, b)| b.define.is_some())
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
    fn reset<F>(&self, changef: F, ctx: &DispCtx)
    where
        F: Fn(BarN, &BarDefine, u64),
    {
        self.for_each(|barn, def| {
            if def == &BarDefine::Mmio64High {
                // The high portion of 64-bit BARs does not require direct
                // handling, as the low portion bears the necessary information.
                return;
            }
            let mut state = self.entries[barn as usize].state.lock().unwrap();
            if state.registered {
                match def {
                    BarDefine::Pio(_) => {
                        ctx.mctx.pio().unregister(state.addr as u16).unwrap();
                    }
                    BarDefine::Mmio(_) | BarDefine::Mmio64(_) => {
                        ctx.mctx
                            .mmio()
                            .unregister(state.addr as usize)
                            .unwrap();
                    }
                    // Already filtered out
                    BarDefine::Mmio64High => panic!(),
                }
                state.registered = false;
                changef(barn, def, state.addr);
            }
            state.addr = 0;
        });
    }
}

struct Cap {
    id: u8,
    offset: u8,
}

pub struct DeviceInst {
    ident: Ident,
    lintr_req: bool,
    cfg_space: RegMap<CfgReg>,
    msix_cfg: Option<Arc<MsixCfg>>,
    caps: Vec<Cap>,

    state: Mutex<State>,
    bars: Bars,
    cond: Condvar,

    sa_cell: SelfArcCell<Self>,

    inner: Arc<dyn Device>,
    // Keep a 'dyn Any' copy around for downcasting
    inner_any: Arc<dyn Any + Send + Sync + 'static>,
}

impl DeviceInst {
    fn new<D>(
        ident: Ident,
        cfg_space: RegMap<CfgReg>,
        msix_cfg: Option<Arc<MsixCfg>>,
        caps: Vec<Cap>,
        bars: Bars,
        inner: Arc<D>,
    ) -> Self
    where
        D: Device + Send + Sync + 'static,
    {
        let inner_any =
            Arc::clone(&inner) as Arc<dyn Any + Send + Sync + 'static>;
        Self {
            ident,
            lintr_req: false,
            cfg_space,
            msix_cfg,
            caps,

            state: Mutex::new(State {
                reg_intr_line: 0xff,
                ..Default::default()
            }),
            bars,
            cond: Condvar::new(),

            sa_cell: SelfArcCell::new(),

            inner: inner as Arc<dyn Device>,
            inner_any,
        }
    }

    /// State changes which result in a new interrupt mode for the device incur
    /// a notification which could trigger deadlock if normal lock-ordering was
    /// used.  In such cases, the process is done in two stages: the state
    /// update (under lock) and the notification (outside the lock) with
    /// protection provided against other such updates which might race.
    fn affects_intr_mode(
        &self,
        mut state: MutexGuard<State>,
        f: impl FnOnce(&mut State),
    ) {
        state = self.cond.wait_while(state, |s| s.update_in_progress).unwrap();
        f(&mut state);
        let next_mode = self.next_intr_mode(&state);

        state.update_in_progress = true;
        drop(state);
        // inner is notified of mode change w/o state locked
        self.inner.interrupt_mode_change(next_mode);

        let mut state = self.state.lock().unwrap();
        assert!(state.update_in_progress);
        state.update_in_progress = false;
        self.cond.notify_all();
    }

    fn cfg_std_read(&self, id: &StdCfgReg, ro: &mut ReadOp, _ctx: &DispCtx) {
        assert!(ro.offset() == 0 || *id == StdCfgReg::Reserved);

        match id {
            StdCfgReg::VendorId => ro.write_u16(self.ident.vendor_id),
            StdCfgReg::DeviceId => ro.write_u16(self.ident.device_id),
            StdCfgReg::Class => ro.write_u8(self.ident.class),
            StdCfgReg::Subclass => ro.write_u8(self.ident.subclass),
            StdCfgReg::SubVendorId => ro.write_u16(self.ident.sub_vendor_id),
            StdCfgReg::SubDeviceId => ro.write_u16(self.ident.sub_device_id),
            StdCfgReg::ProgIf => ro.write_u8(self.ident.prog_if),
            StdCfgReg::RevisionId => ro.write_u8(self.ident.revision_id),

            StdCfgReg::Command => {
                let val = self.state.lock().unwrap().reg_command.bits();
                ro.write_u16(val);
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
                if !self.caps.is_empty() {
                    val.insert(RegStatus::CAP_LIST);
                }
                ro.write_u16(val.bits());
            }
            StdCfgReg::IntrLine => {
                ro.write_u8(self.state.lock().unwrap().reg_intr_line)
            }
            StdCfgReg::IntrPin => {
                ro.write_u8(self.state.lock().unwrap().reg_intr_pin)
            }
            StdCfgReg::Bar(bar) => ro.write_u32(self.bars.reg_read(*bar)),
            StdCfgReg::ExpansionRomAddr => {
                // no rom for now
                ro.write_u32(0);
            }
            StdCfgReg::CapPtr => {
                if !self.caps.is_empty() {
                    ro.write_u8(self.caps[0].offset);
                } else {
                    ro.write_u8(0);
                }
            }
            StdCfgReg::HeaderType => {
                // TODO: add multi-function and other bits
                ro.write_u8(0);
            }
            StdCfgReg::Reserved => {
                ro.fill(0);
            }
            StdCfgReg::CacheLineSize
            | StdCfgReg::LatencyTimer
            | StdCfgReg::MaxLatency
            | StdCfgReg::Bist
            | StdCfgReg::MinGrant
            | StdCfgReg::CardbusPtr => {
                // XXX: zeroed for now
                ro.fill(0);
            }
        }
    }
    fn cfg_std_write(&self, id: &StdCfgReg, wo: &mut WriteOp, ctx: &DispCtx) {
        assert!(wo.offset() == 0 || *id == StdCfgReg::Reserved);

        match id {
            StdCfgReg::Command => {
                let new = RegCmd::from_bits_truncate(wo.read_u16());
                self.reg_cmd_write(new, ctx);
            }
            StdCfgReg::IntrLine => {
                self.state.lock().unwrap().reg_intr_line = wo.read_u8();
            }
            StdCfgReg::Bar(bar) => {
                let val = wo.read_u32();
                let state = self.state.lock().unwrap();
                self.bars.reg_write(*bar, val, |def, old, new| {
                    // fail move for now
                    match def {
                        BarDefine::Pio(sz) => {
                            if !state.reg_command.contains(RegCmd::IO_EN) {
                                // pio mappings are disabled via cmd reg
                                return false;
                            }
                            let bus = ctx.mctx.pio();
                            // We know this was previously registered
                            let (dev, old_bar) =
                                bus.unregister(old as u16).unwrap();
                            assert_eq!(old_bar, *bar as usize);
                            bus.register(new as u16, *sz, dev, *bar as usize)
                                .is_err()
                        }
                        BarDefine::Mmio(_) | BarDefine::Mmio64(_) => {
                            if !state.reg_command.contains(RegCmd::MMIO_EN) {
                                // mmio mappings are disabled via cmd reg
                                return false;
                            }
                            let sz = match def {
                                BarDefine::Mmio(s) => *s as usize,
                                BarDefine::Mmio64(s) => *s as usize,
                                _ => panic!(),
                            };
                            let bus = ctx.mctx.mmio();
                            // We know this was previously registered
                            let (dev, old_bar) =
                                bus.unregister(old as usize).unwrap();
                            assert_eq!(old_bar, *bar as usize);
                            bus.register(new as usize, sz, dev, *bar as usize)
                                .is_err()
                        }
                        BarDefine::Mmio64High => panic!(),
                    }
                });
            }
            StdCfgReg::VendorId
            | StdCfgReg::DeviceId
            | StdCfgReg::Class
            | StdCfgReg::Subclass
            | StdCfgReg::SubVendorId
            | StdCfgReg::SubDeviceId
            | StdCfgReg::HeaderType
            | StdCfgReg::ProgIf
            | StdCfgReg::RevisionId
            | StdCfgReg::CapPtr
            | StdCfgReg::IntrPin
            | StdCfgReg::Reserved => {
                // ignore writes to RO fields
            }
            StdCfgReg::ExpansionRomAddr => {
                // no expansion rom for now
            }
            StdCfgReg::Status => {
                // Treat status register as RO until there is a need for guests
                // to clear bits within it
            }
            StdCfgReg::CacheLineSize
            | StdCfgReg::LatencyTimer
            | StdCfgReg::MaxLatency
            | StdCfgReg::Bist
            | StdCfgReg::MinGrant
            | StdCfgReg::CardbusPtr => {
                // XXX: ignored for now
            }
        }
    }
    fn reg_cmd_write(&self, val: RegCmd, ctx: &DispCtx) {
        let mut state = self.state.lock().unwrap();
        let diff = val ^ state.reg_command;
        self.update_bar_registration(diff, val, ctx);
        if diff.intersects(RegCmd::INTX_DIS) {
            // special handling required for INTx enable/disable
            self.affects_intr_mode(state, |state| {
                state.reg_command = val;
            });
        } else {
            state.reg_command = val;
        }
    }

    fn next_intr_mode(&self, state: &State) -> IntrMode {
        if self.msix_cfg.is_some()
            && self.msix_cfg.as_ref().unwrap().is_enabled()
        {
            return IntrMode::Msix;
        }
        if state.lintr_pin.is_some()
            && !state.reg_command.contains(RegCmd::INTX_DIS)
        {
            return IntrMode::INTxPin;
        }

        IntrMode::Disabled
    }

    fn update_bar_registration(
        &self,
        diff: RegCmd,
        new: RegCmd,
        ctx: &DispCtx,
    ) {
        if !diff.intersects(RegCmd::IO_EN | RegCmd::MMIO_EN) {
            return;
        }

        self.bars.change_registrations(
            |bar, def, addr, registered| match def {
                BarDefine::Pio(sz) => {
                    if !diff.intersects(RegCmd::IO_EN) {
                        return None;
                    }

                    if registered && !new.contains(RegCmd::IO_EN) {
                        ctx.mctx.pio().unregister(addr as u16).unwrap();
                        return Some(false);
                    } else if !registered && new.contains(RegCmd::IO_EN) {
                        let reg_attempt = ctx
                            .mctx
                            .pio()
                            .register(
                                addr as u16,
                                *sz as u16,
                                self.self_weak(),
                                bar as usize,
                            )
                            .is_ok();
                        return Some(reg_attempt);
                    }
                    None
                }
                BarDefine::Mmio(_) | BarDefine::Mmio64(_) => {
                    if !diff.intersects(RegCmd::MMIO_EN) {
                        return None;
                    }

                    let sz = match def {
                        BarDefine::Mmio(s) => *s as u64,
                        BarDefine::Mmio64(s) => *s,
                        _ => panic!(),
                    };

                    if registered && !new.contains(RegCmd::IO_EN) {
                        ctx.mctx.mmio().unregister(addr as usize).unwrap();
                        return Some(false);
                    } else if !registered && new.contains(RegCmd::IO_EN) {
                        let reg_attempt = ctx
                            .mctx
                            .mmio()
                            .register(
                                addr as usize,
                                sz as usize,
                                self.self_weak(),
                                bar as usize,
                            )
                            .is_ok();
                        return Some(reg_attempt);
                    }

                    None
                }
                // Registration for the high portion of a 64-bit BAR is not
                // handled separately.
                BarDefine::Mmio64High => panic!(),
            },
        );
    }
    fn bar_rw(&self, ident: usize, rwo: RWOp, ctx: &DispCtx) {
        let bar = BarN::try_from(ident as u8).unwrap();
        if let Some(msix) = self.msix_cfg.as_ref() {
            if msix.bar_match(bar) {
                msix.bar_rw(rwo, |info| self.notify_msi_update(info, ctx), ctx);
                return;
            }
        }
        self.inner.bar_rw(bar, rwo, ctx);
    }

    fn cfg_cap_rw(&self, id: &CfgReg, rwo: RWOp, ctx: &DispCtx) {
        match id {
            CfgReg::CapId(i) => {
                if let RWOp::Read(ro) = rwo {
                    ro.write_u8(self.caps[*i as usize].id)
                }
            }
            CfgReg::CapNext(i) => {
                if let RWOp::Read(ro) = rwo {
                    let next = *i as usize + 1;
                    if next < self.caps.len() {
                        ro.write_u8(self.caps[next].offset);
                    } else {
                        ro.write_u8(0);
                    }
                }
            }
            CfgReg::CapBody(i) => self.do_cap_rw(*i, rwo, ctx),

            // Should be filtered down to only cap regs by now
            _ => panic!(),
        }
    }
    fn do_cap_rw(&self, idx: u8, rwo: RWOp, ctx: &DispCtx) {
        assert!(idx < self.caps.len() as u8);
        // XXX: no fancy capability support for now
        let cap = &self.caps[idx as usize];
        match cap.id {
            CAP_ID_MSIX => {
                let msix_cfg = self.msix_cfg.as_ref().unwrap();
                if let RWOp::Write(_) = rwo {
                    // MSI-X cap writes may result in a change to the interrupt
                    // mode of the device which requires extra locking concerns.
                    let state = self.state.lock().unwrap();
                    self.affects_intr_mode(state, |_state| {
                        msix_cfg.cfg_rw(
                            rwo,
                            |info| self.notify_msi_update(info, ctx),
                            ctx,
                        );
                    });
                } else {
                    msix_cfg.cfg_rw(
                        rwo,
                        |info| self.notify_msi_update(info, ctx),
                        ctx,
                    );
                }
            }
            _ => {
                println!(
                    "unhandled cap access id:{:x} off:{:x}",
                    cap.id,
                    rwo.offset()
                );
            }
        }
    }
    fn notify_msi_update(&self, info: MsiUpdate, ctx: &DispCtx) {
        self.inner.msi_update(info, ctx);
    }
    /// Get access to the inner device emulation.
    ///
    /// This will panic if the provided type does not match.
    pub fn inner_dev<T: Send + Sync + 'static>(&self) -> Arc<T> {
        let inner = Arc::clone(&self.inner_any);
        Arc::downcast(inner).unwrap()
    }
    fn do_reset(&self, ctx: &DispCtx) {
        let state = self.state.lock().unwrap();
        self.affects_intr_mode(state, |state| {
            state.reg_command.reset();
            if let Some(msix) = &self.msix_cfg {
                msix.reset();
            }
        });
        self.bars.reset(
            |_bar, _def, _addr| {
                // TODO: notify device of unregistered BARs
            },
            ctx,
        );
    }
}

impl Endpoint for DeviceInst {
    fn cfg_rw(&self, mut rwo: RWOp, ctx: &DispCtx) {
        self.cfg_space.process(&mut rwo, |id, mut rwo| match id {
            CfgReg::Std => {
                STD_CFG_MAP.process(&mut rwo, |id, rwo| match rwo {
                    RWOp::Read(ro) => self.cfg_std_read(id, ro, ctx),
                    RWOp::Write(wo) => self.cfg_std_write(id, wo, ctx),
                });
            }
            CfgReg::Custom(region) => self.inner.cfg_rw(*region, rwo),
            CfgReg::CapId(_) | CfgReg::CapNext(_) | CfgReg::CapBody(_) => {
                self.cfg_cap_rw(id, rwo, ctx)
            }
        });
    }
    fn attach(&self, get_lintr: &dyn Fn() -> (INTxPinID, Arc<dyn IntrPin>)) {
        let mut state = self.state.lock().unwrap();
        if self.lintr_req {
            let (intx, isa_pin) = get_lintr();
            state.reg_intr_pin = intx as u8;
            state.lintr_pin = Some(isa_pin);
        }
        drop(state);

        let lintr_pin = match self.lintr_req {
            true => Some(INTxPin::new(self.self_weak())),
            false => None,
        };
        let msix_hdl = self.msix_cfg.as_ref().map(|msix| MsixHdl::new(msix));
        self.inner.attach(lintr_pin, msix_hdl);
    }

    fn bar_for_each(&self, cb: &mut dyn FnMut(BarN, &BarDefine)) {
        self.bars.for_each(cb)
    }

    fn bar_place(&self, bar: BarN, addr: u64) {
        // Expect that IO/MMIO is disabled while we are placing BARs
        let state = self.state.lock().unwrap();
        assert!(state.reg_command == RegCmd::INTX_DIS);

        self.bars.place(bar, addr);
    }
    fn as_devinst(&self) -> Option<&DeviceInst> {
        Some(self)
    }
}

impl PioDev for DeviceInst {
    fn pio_rw(&self, _port: u16, ident: usize, rwo: RWOp, ctx: &DispCtx) {
        self.bar_rw(ident, rwo, ctx);
    }
}
impl MmioDev for DeviceInst {
    fn mmio_rw(&self, _addr: usize, ident: usize, rwo: RWOp, ctx: &DispCtx) {
        self.bar_rw(ident, rwo, ctx);
    }
}

impl Entity for DeviceInst {
    fn state_transition(
        &self,
        next: instance::State,
        target: Option<instance::State>,
        ctx: &DispCtx,
    ) {
        if matches!(next, instance::State::Reset) {
            self.do_reset(ctx);
        }
        self.inner.state_transition(next, target, ctx);
    }
}

impl SelfArc for DeviceInst {
    fn self_arc_cell(&self) -> &SelfArcCell<Self> {
        &self.sa_cell
    }
}

#[derive(Clone)]
pub struct INTxPin {
    outer: Weak<DeviceInst>,
}
impl INTxPin {
    fn new(outer: Weak<DeviceInst>) -> Self {
        Self { outer }
    }
    pub fn assert(&self) {
        self.with_pin(|pin| pin.assert());
    }
    pub fn deassert(&self) {
        self.with_pin(|pin| pin.deassert());
    }
    pub fn pulse(&self) {
        self.with_pin(|pin| pin.pulse());
    }
    fn with_pin(&self, f: impl FnOnce(&dyn IntrPin)) {
        if let Some(dev) = Weak::upgrade(&self.outer) {
            let state = dev.state.lock().unwrap();
            f(state.lintr_pin.as_ref().unwrap().as_ref());
        }
    }
}

#[derive(Copy, Clone, Eq, PartialEq)]
pub enum IntrMode {
    Disabled,
    INTxPin,
    Msix,
}

pub enum MsiUpdate {
    MaskAll,
    UnmaskAll,
    Modify(u16),
}

pub trait Device: Send + Sync + 'static + Entity {
    #[allow(unused_variables)]
    fn bar_rw(&self, bar: BarN, rwo: RWOp, ctx: &DispCtx) {
        match rwo {
            RWOp::Read(ro) => {
                unimplemented!("BAR read ({:?} @ {:x})", bar, ro.offset())
            }
            RWOp::Write(wo) => {
                unimplemented!("BAR write ({:?} @ {:x})", bar, wo.offset())
            }
        }
    }

    fn cfg_rw(&self, region: u8, rwo: RWOp) {
        match rwo {
            RWOp::Read(ro) => {
                unimplemented!("CFG read ({:x} @ {:x})", region, ro.offset())
            }
            RWOp::Write(wo) => {
                unimplemented!("CFG write ({:x} @ {:x})", region, wo.offset())
            }
        }
    }
    fn attach(&self, lintr_pin: Option<INTxPin>, msix_hdl: Option<MsixHdl>) {
        // A device model has no reason to request interrupt resources but not
        // make use of them.
        assert!(lintr_pin.is_none());
        assert!(msix_hdl.is_none());
    }
    #[allow(unused_variables)]
    fn interrupt_mode_change(&self, mode: IntrMode) {}
    #[allow(unused_variables)]
    fn msi_update(&self, info: MsiUpdate, ctx: &DispCtx) {}
    // TODO
    // fn cap_read(&self);
    // fn cap_write(&self);
}

enum MsixBarReg {
    Addr(u16),
    Data(u16),
    VecCtrl(u16),
    Reserved,
    Pba,
}
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum MsixCapReg {
    MsgCtrl,
    TableOff,
    PbaOff,
}
lazy_static! {
    static ref CAP_MSIX_MAP: RegMap<MsixCapReg> = {
        let layout = [
            (MsixCapReg::MsgCtrl, 2),
            (MsixCapReg::TableOff, 4),
            (MsixCapReg::PbaOff, 4),
        ];
        RegMap::create_packed(10, &layout, None)
    };
}

const MSIX_VEC_MASK: u32 = 1 << 0;

const MSIX_MSGCTRL_ENABLE: u16 = 1 << 15;
const MSIX_MSGCTRL_FMASK: u16 = 1 << 14;

#[derive(Default)]
struct MsixEntry {
    addr: u64,
    data: u32,
    mask_vec: bool,
    mask_func: bool,
    enabled: bool,
    pending: bool,
}
impl MsixEntry {
    fn fire(&mut self, ctx: &DispCtx) {
        if !self.enabled {
            return;
        }
        if self.mask_func || self.mask_vec {
            self.pending = true;
            return;
        }
        ctx.mctx.hdl().lapic_msi(self.addr, self.data as u64).unwrap();
    }
    fn check_mask(&mut self, ctx: &DispCtx) {
        if !self.mask_vec && !self.mask_func && self.pending {
            self.pending = false;
            ctx.mctx.hdl().lapic_msi(self.addr, self.data as u64).unwrap();
        }
    }
    fn reset(&mut self) {
        self.addr = 0;
        self.data = 0;
        self.mask_vec = false;
        self.mask_func = false;
        self.enabled = false;
        self.pending = false;
    }
}

struct MsixCfg {
    count: u16,
    bar: BarN,
    pba_off: u32,
    map: RegMap<MsixBarReg>,
    entries: Vec<Mutex<MsixEntry>>,
    state: Mutex<MsixCfgState>,
}
#[derive(Default)]
struct MsixCfgState {
    enabled: bool,
    func_mask: bool,
}
impl MsixCfg {
    fn new(count: u16, bar: BarN) -> (Arc<Self>, usize) {
        assert!(count > 0 && count <= 2048);

        // Pad table so PBA is on a separate page.  This will allow the guest
        // to map it separately, should it so choose.
        let table_size = count as usize * 16;
        let table_pad = match table_size % PAGE_SIZE {
            0 => 0,
            a => PAGE_SIZE - a,
        };

        // With a maximum vector count, the PBA will not require more than a
        // page.  For convenience, pad it out to that size.
        let pba_size = PAGE_SIZE;

        let pba_off = table_size + table_pad;
        let bar_size = (pba_off + pba_size).next_power_of_two();

        let mut map = RegMap::new(bar_size);
        let mut off = 0;
        for i in 0..count {
            map.define(off, 8, MsixBarReg::Addr(i));
            map.define(off + 8, 4, MsixBarReg::Data(i));
            map.define(off + 12, 4, MsixBarReg::VecCtrl(i));
            off += 16;
        }
        if table_pad != 0 {
            map.define_with_flags(
                off,
                table_pad,
                MsixBarReg::Reserved,
                Flags::PASSTHRU,
            );
        }
        off += table_pad;
        map.define_with_flags(off, pba_size, MsixBarReg::Pba, Flags::PASSTHRU);
        off += pba_size;

        // If table sizing leaves space after the PBA in order to pad the BAR
        // out to the next power of 2, cover it with Reserved handling.
        if off < bar_size {
            let pba_pad = bar_size - off;
            map.define_with_flags(
                off,
                pba_pad,
                MsixBarReg::Reserved,
                Flags::PASSTHRU,
            );
        }

        let mut entries = Vec::with_capacity(count as usize);
        entries.resize_with(count as usize, Default::default);

        let this = Self {
            count,
            bar,
            pba_off: pba_off as u32,
            map,
            entries,
            state: Default::default(),
        };

        (Arc::new(this), bar_size)
    }
    fn bar_match(&self, bar: BarN) -> bool {
        self.bar == bar
    }
    fn bar_rw(
        &self,
        mut rwo: RWOp,
        updatef: impl Fn(MsiUpdate),
        ctx: &DispCtx,
    ) {
        self.map.process(&mut rwo, |id, rwo| match rwo {
            RWOp::Read(ro) => match id {
                MsixBarReg::Addr(i) => {
                    let ent = self.entries[*i as usize].lock().unwrap();
                    ro.write_u64(ent.addr);
                }
                MsixBarReg::Data(i) => {
                    let ent = self.entries[*i as usize].lock().unwrap();
                    ro.write_u32(ent.data);
                }
                MsixBarReg::VecCtrl(i) => {
                    let ent = self.entries[*i as usize].lock().unwrap();
                    let mut val = 0;
                    if ent.mask_vec {
                        val |= MSIX_VEC_MASK;
                    }
                    ro.write_u32(val);
                }
                MsixBarReg::Reserved => {
                    ro.fill(0);
                }
                MsixBarReg::Pba => {
                    self.read_pba(ro);
                }
            },
            RWOp::Write(wo) => {
                // If modifying an individual entry, its lock needs to be dropped before making
                // the `updatef` callback, since it may attempt to access the entry itself.  To
                // synchronize access, hold on to the state lock across that call.
                let state = self.state.lock().unwrap();
                match id {
                    MsixBarReg::Addr(i) => {
                        let mut ent = self.entries[*i as usize].lock().unwrap();
                        ent.addr = wo.read_u64();
                        drop(ent);
                        updatef(MsiUpdate::Modify(*i));
                    }
                    MsixBarReg::Data(i) => {
                        let mut ent = self.entries[*i as usize].lock().unwrap();
                        ent.data = wo.read_u32();
                        drop(ent);
                        updatef(MsiUpdate::Modify(*i));
                    }
                    MsixBarReg::VecCtrl(i) => {
                        let mut ent = self.entries[*i as usize].lock().unwrap();
                        let val = wo.read_u32();
                        ent.mask_vec = val & MSIX_VEC_MASK != 0;
                        ent.check_mask(ctx);
                        drop(ent);
                        updatef(MsiUpdate::Modify(*i));
                    }
                    MsixBarReg::Reserved | MsixBarReg::Pba => {}
                }
                drop(state);
            }
        });
    }
    fn read_pba(&self, ro: &mut ReadOp) {
        let avail = ro.avail();
        let offset = ro.offset();

        for i in 0..avail {
            let mut val: u8 = 0;
            for bitpos in 0..8 {
                let idx = ((i + offset) * 8) + bitpos;
                if idx < self.count as usize {
                    let ent = self.entries[idx].lock().unwrap();
                    if ent.pending {
                        val |= 1 << bitpos;
                    }
                }
            }
            ro.write_u8(val);
        }
    }
    fn cfg_rw(
        &self,
        mut rwo: RWOp,
        updatef: impl Fn(MsiUpdate),
        ctx: &DispCtx,
    ) {
        CAP_MSIX_MAP.process(&mut rwo, |id, rwo| {
            match rwo {
                RWOp::Read(ro) => {
                    match id {
                        MsixCapReg::MsgCtrl => {
                            let state = self.state.lock().unwrap();
                            // low 10 bits hold `count - 1`
                            let mut val = self.count as u16 - 1;
                            if state.enabled {
                                val |= MSIX_MSGCTRL_ENABLE;
                            }
                            if state.func_mask {
                                val |= MSIX_MSGCTRL_FMASK;
                            }
                            ro.write_u16(val);
                        }
                        MsixCapReg::TableOff => {
                            // table always at offset 0 for now
                            ro.write_u32(self.bar as u8 as u32);
                        }
                        MsixCapReg::PbaOff => {
                            ro.write_u32(self.pba_off | self.bar as u8 as u32);
                        }
                    }
                }
                RWOp::Write(wo) => {
                    match id {
                        MsixCapReg::MsgCtrl => {
                            let val = wo.read_u16();
                            let mut state = self.state.lock().unwrap();
                            let new_ena = val & MSIX_MSGCTRL_ENABLE != 0;
                            let old_ena = state.enabled;
                            let new_mask = val & MSIX_MSGCTRL_FMASK != 0;
                            let old_mask = state.func_mask;
                            if old_ena != new_ena || old_mask != new_mask {
                                self.each_entry(|ent| {
                                    ent.mask_func = new_mask;
                                    ent.enabled = new_ena;
                                    ent.check_mask(ctx);
                                });
                            }
                            state.enabled = new_ena;
                            state.func_mask = new_mask;

                            // Notify when the MSI-X function mask is changing.  Changes to
                            // enable/disable state is already covered by the logic for
                            // interrupt_mode_change updates
                            if old_mask != new_mask
                                && old_ena == new_ena
                                && new_ena
                            {
                                updatef(match new_mask {
                                    true => MsiUpdate::MaskAll,
                                    false => MsiUpdate::UnmaskAll,
                                });
                            }
                        }
                        // only msgctrl can be written
                        _ => {}
                    }
                }
            }
        });
    }
    fn each_entry(&self, mut cb: impl FnMut(&mut MsixEntry)) {
        for ent in self.entries.iter() {
            let mut locked = ent.lock().unwrap();
            cb(&mut locked)
        }
    }
    fn fire(&self, idx: u16, ctx: &DispCtx) {
        assert!(idx < self.count);
        let mut ent = self.entries[idx as usize].lock().unwrap();
        ent.fire(ctx);
    }
    fn is_enabled(&self) -> bool {
        let state = self.state.lock().unwrap();
        state.enabled
    }
    fn read(&self, idx: u16) -> MsiEnt {
        assert!(idx < self.count);
        let ent = self.entries[idx as usize].lock().unwrap();
        MsiEnt {
            addr: ent.addr,
            data: ent.data,
            masked: ent.mask_vec || ent.mask_func,
            pending: ent.pending,
        }
    }
    fn reset(&self) {
        let mut state = self.state.lock().unwrap();
        state.enabled = false;
        state.func_mask = false;
        drop(state);
        self.each_entry(|ent| ent.reset());
    }
}

// public struct for exposing MSI(-X) values
pub struct MsiEnt {
    pub addr: u64,
    pub data: u32,
    pub masked: bool,
    pub pending: bool,
}

pub struct MsixHdl {
    cfg: Arc<MsixCfg>,
}
impl MsixHdl {
    fn new(cfg: &Arc<MsixCfg>) -> Self {
        Self { cfg: Arc::clone(cfg) }
    }
    pub fn fire(&self, idx: u16, ctx: &DispCtx) {
        self.cfg.fire(idx, ctx);
    }
    pub fn read(&self, idx: u16) -> MsiEnt {
        self.cfg.read(idx)
    }
    pub fn count(&self) -> u16 {
        self.cfg.count
    }
}
impl Clone for MsixHdl {
    fn clone(&self) -> Self {
        Self { cfg: Arc::clone(&self.cfg) }
    }
}

pub struct Builder<I> {
    ident: Ident,
    lintr_req: bool,
    msix_cfg: Option<Arc<MsixCfg>>,
    bars: [Option<BarDefine>; 6],
    cfgmap: RegMap<CfgReg>,

    cap_next_alloc: usize,
    caps: Vec<Cap>,

    _phantom: PhantomData<I>,
}

impl<I: Device + 'static> Builder<I> {
    pub fn new(ident: Ident) -> Self {
        let mut cfgmap = RegMap::new(LEN_CFG);
        cfgmap.define_with_flags(0, LEN_CFG_STD, CfgReg::Std, Flags::PASSTHRU);
        Self {
            ident,
            lintr_req: false,
            msix_cfg: None,
            bars: [None; 6],
            cfgmap,

            caps: Vec::new(),
            // capabilities can start immediately after std cfg area
            cap_next_alloc: LEN_CFG_STD,

            _phantom: PhantomData,
        }
    }

    /// Add a BAR which is accessible via IO ports
    ///
    /// # Panics
    ///
    /// If `size` is < 4 or not a power of 2.
    pub fn add_bar_io(mut self, bar: BarN, size: u16) -> Self {
        assert!(size.is_power_of_two());
        assert!(size >= 4);

        let idx = bar as usize;
        assert!(self.bars[idx].is_none());

        self.bars[idx] = Some(BarDefine::Pio(size));
        self
    }

    /// Add a BAR which is accessible via MMIO.  The size and placement of the
    /// BAR is limited to the 32-bit address space.
    ///
    /// # Panics
    ///
    /// If `size` is < 16 or not a power of 2.
    pub fn add_bar_mmio(mut self, bar: BarN, size: u32) -> Self {
        assert!(size.is_power_of_two());
        assert!(size >= 16);

        let idx = bar as usize;
        assert!(self.bars[idx].is_none());

        self.bars[idx] = Some(BarDefine::Mmio(size));
        self
    }

    /// Add a BAR which is accessible via MMIO.  As a 64-bit BAR, its size can
    /// be >= 4G, and it is expected to be placed above the 32-bit address
    /// limit.
    ///
    /// # Panics
    ///
    /// If `size` is < 16 or not a power of 2.
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

    /// Add a legacy (pin-based) interrupt
    pub fn add_lintr(mut self) -> Self {
        self.lintr_req = true;
        self
    }

    /// Add a region of the PCI config space for the device which has custom
    /// handling.
    pub fn add_custom_cfg(mut self, offset: u8, len: u8) -> Self {
        self.cfgmap.define_with_flags(
            offset as usize,
            len as usize,
            CfgReg::Custom(offset),
            Flags::PASSTHRU,
        );
        self
    }

    fn add_cap_raw(&mut self, id: u8, len: u8) {
        // XXX: does not pay heed to any custom cfg sections which are added via
        // the `add_custom_cfg` interface.
        let end = self.cap_next_alloc + 2 + len as usize;
        // XXX: on the caller to size properly for alignment requirements
        assert!(end % 4 == 0);
        assert!(end <= u8::MAX as usize);
        let idx = self.caps.len() as u8;
        self.caps.push(Cap { id, offset: self.cap_next_alloc as u8 });
        self.cfgmap.define(self.cap_next_alloc, 1, CfgReg::CapId(idx));
        self.cfgmap.define(self.cap_next_alloc + 1, 1, CfgReg::CapNext(idx));
        self.cfgmap.define(
            self.cap_next_alloc + 2,
            len as usize,
            CfgReg::CapBody(idx),
        );
        self.cap_next_alloc = end;
    }

    /// Add MSI-X interrupt functionality.
    ///
    /// # Panics
    ///
    /// If:
    /// - `count` is 0 or > 2048
    /// - `bar` conflicts (overlaps) with other defined BAR for the device
    pub fn add_cap_msix(mut self, bar: BarN, count: u16) -> Self {
        assert!(self.msix_cfg.is_none());

        let (cfg, bar_size) = MsixCfg::new(count, bar);

        assert!(bar_size < u32::MAX as usize);
        self = self.add_bar_mmio(bar, bar_size as u32);
        self.msix_cfg = Some(cfg);
        self.add_cap_raw(CAP_ID_MSIX, 10);

        self
    }

    pub fn finish(self, inner: Arc<I>) -> Arc<DeviceInst> {
        let bars = Bars::new(&self.bars);

        let mut inst = DeviceInst::new(
            self.ident,
            self.cfgmap,
            self.msix_cfg,
            self.caps,
            bars,
            inner,
        );
        inst.lintr_req = self.lintr_req;

        let mut done = Arc::new(inst);
        SelfArc::self_arc_init(&mut done);
        done
    }
}

#[cfg(test)]
mod test {
    use super::*;

    fn bar_setup() -> Bars {
        let bar_defs = [
            Some(BarDefine::Pio(0x100)),
            Some(BarDefine::Mmio(0x20000)),
            Some(BarDefine::Mmio64(0x40000)),
            Some(BarDefine::Mmio64High),
            Some(BarDefine::Mmio64(0x200000000)),
            Some(BarDefine::Mmio64High),
        ];
        let bars = Bars::new(&bar_defs);
        bars.place(BarN::BAR0, 0x1000);
        bars.place(BarN::BAR1, 0xc000000);
        bars.place(BarN::BAR2, 0xd000000);
        bars.place(BarN::BAR4, 0x800000000);

        bars
    }
    #[test]
    fn bar_init() {
        let _ = bar_setup();
    }

    #[test]
    fn bar_limits() {
        let bars = bar_setup();

        assert_eq!(bars.reg_read(BarN::BAR0), 0x1001);
        assert_eq!(bars.reg_read(BarN::BAR1), 0xc000000);
        assert_eq!(bars.reg_read(BarN::BAR2), 0xd000004);
        assert_eq!(bars.reg_read(BarN::BAR3), 0);
        assert_eq!(bars.reg_read(BarN::BAR4), 0x4);
        assert_eq!(bars.reg_read(BarN::BAR5), 0x8);
        for i in 0..=5u8 {
            bars.reg_write(
                BarN::try_from(i).unwrap(),
                0xffffffff,
                |_, _, _| false,
            );
        }
        assert_eq!(bars.reg_read(BarN::BAR0), 0x0000ff01);
        assert_eq!(bars.reg_read(BarN::BAR1), 0xfffe0000);
        assert_eq!(bars.reg_read(BarN::BAR2), 0xfffc0004);
        assert_eq!(bars.reg_read(BarN::BAR3), 0xffffffff);
        assert_eq!(bars.reg_read(BarN::BAR4), 0x00000004);
        assert_eq!(bars.reg_read(BarN::BAR5), 0xfffffffe);
    }

    #[test]
    #[should_panic]
    fn msix_cfg_zero() {
        let (_cfg, _bsize) = MsixCfg::new(0, BarN::BAR1);
    }
    #[test]
    #[should_panic]
    fn msix_cfg_too_big() {
        let (_cfg, _bsize) = MsixCfg::new(2049, BarN::BAR1);
    }
    #[test]
    fn msix_cfg_sizing() {
        let (_cfg, bar_size) = MsixCfg::new(2048, BarN::BAR1);
        // 32k for entries + 4k PBA -> 64k (rounded to next pow2)
        assert_eq!(bar_size, 65536);

        // 4k for entries + 4k PBA
        let (_cfg, bar_size) = MsixCfg::new(256, BarN::BAR1);
        assert_eq!(bar_size, 8192);
    }
}
