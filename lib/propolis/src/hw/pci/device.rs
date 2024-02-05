// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::sync::{Arc, Condvar, Mutex, MutexGuard};

use super::bar::{BarDefine, Bars};
use super::bits::*;
use super::cfgspace::{CfgBuilder, CfgReg};
use super::{bus, BarN, Endpoint};
use crate::accessors::{MemAccessor, MsiAccessor};
use crate::common::*;
use crate::intr_pins::IntrPin;
use crate::migrate::*;
use crate::util::regmap::{Flags, RegMap};

use lazy_static::lazy_static;

pub trait Device: Send + Sync + 'static {
    fn device_state(&self) -> &DeviceState;

    #[allow(unused_variables)]
    fn bar_rw(&self, bar: BarN, rwo: RWOp) {
        match rwo {
            RWOp::Read(ro) => {
                unimplemented!("BAR read ({:?} @ {:x})", bar, ro.offset())
            }
            RWOp::Write(wo) => {
                unimplemented!("BAR write ({:?} @ {:x})", bar, wo.offset())
            }
        }
    }
    #[allow(unused_variables)]
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
    fn attach(&self) {}
    #[allow(unused_variables)]
    fn interrupt_mode_change(&self, mode: IntrMode) {}
    #[allow(unused_variables)]
    fn msi_update(&self, info: MsiUpdate) {}
    // TODO
    // fn cap_read(&self);
    // fn cap_write(&self);
}

impl<D: Device + Send + Sync + 'static> Endpoint for D {
    fn attach(&self, attachment: bus::Attachment) {
        let ds = self.device_state();
        ds.attach(attachment);
        self.attach();
    }
    fn cfg_rw(&self, mut rwo: RWOp) {
        let ds = self.device_state();
        ds.cfg_space.process(&mut rwo, |id, mut rwo| match id {
            CfgReg::Std => {
                STD_CFG_MAP.process(&mut rwo, |id, rwo| match rwo {
                    RWOp::Read(ro) => ds.cfg_std_read(id, ro),
                    RWOp::Write(wo) => ds.cfg_std_write(self, id, wo),
                });
            }
            CfgReg::Custom(region) => Device::cfg_rw(self, *region, rwo),
            CfgReg::CapId(_) | CfgReg::CapNext(_) | CfgReg::CapBody(_) => {
                ds.cfg_cap_rw(self, id, rwo)
            }
        });
    }
    fn bar_rw(&self, bar: BarN, rwo: RWOp) {
        let ds = self.device_state();
        if let Some(msix) = ds.msix_cfg.as_ref() {
            if msix.bar_match(bar) {
                msix.bar_rw(rwo, |info| ds.notify_msi_update(self, info));
                return;
            }
        }
        Device::bar_rw(self, bar, rwo);
    }
}

#[derive(Copy, Clone, Eq, PartialEq, Debug)]
pub(super) enum StdCfgReg {
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

struct State {
    reg_command: RegCmd,
    reg_intr_line: u8,

    attach: Option<bus::Attachment>,
    bars: Bars,

    update_in_progress: bool,
}
impl State {
    fn new(bars: Bars) -> Self {
        Self {
            reg_command: RegCmd::empty(),
            reg_intr_line: 0xff,
            attach: None,
            bars,
            update_in_progress: false,
        }
    }
    fn attached(&self) -> &bus::Attachment {
        self.attach.as_ref().unwrap()
    }
}

pub(super) struct Cap {
    id: u8,
    offset: u8,
}

impl Cap {
    pub(super) fn new(id: u8, offset: u8) -> Self {
        Self { id, offset }
    }
}

pub struct DeviceState {
    ident: Ident,
    lintr_support: bool,
    cfg_space: RegMap<CfgReg>,
    msix_cfg: Option<Arc<MsixCfg>>,
    caps: Vec<Cap>,

    pub acc_mem: MemAccessor,
    // MSI accessor remains "hidden" behind MsixCfg machinery
    acc_msi: MsiAccessor,

    state: Mutex<State>,
    cond: Condvar,
}

impl DeviceState {
    fn new(
        ident: Ident,
        lintr_support: bool,
        cfg_space: RegMap<CfgReg>,
        msix_cfg: Option<Arc<MsixCfg>>,
        caps: Vec<Cap>,
        bars: Bars,
    ) -> Self {
        let acc_msi = MsiAccessor::new_orphan();
        if let Some(cfg) = msix_cfg.as_ref() {
            cfg.attach(&acc_msi);
        }

        Self {
            ident,
            lintr_support,
            cfg_space,
            msix_cfg,
            caps,

            acc_mem: MemAccessor::new_orphan(),
            acc_msi,

            state: Mutex::new(State::new(bars)),
            cond: Condvar::new(),
        }
    }

    /// State changes which result in a new interrupt mode for the device incur
    /// a notification which could trigger deadlock if normal lock-ordering was
    /// used.  In such cases, the process is done in two stages: the state
    /// update (under lock) and the notification (outside the lock) with
    /// protection provided against other such updates which might race.
    fn affects_intr_mode(
        &self,
        dev: &dyn Device,
        mut state: MutexGuard<State>,
        f: impl FnOnce(&mut State),
    ) -> MutexGuard<State> {
        state = self.cond.wait_while(state, |s| s.update_in_progress).unwrap();
        f(&mut state);
        let next_mode = self.which_intr_mode(&state);

        state.update_in_progress = true;
        drop(state);
        // device is notified of mode change w/o state locked
        dev.interrupt_mode_change(next_mode);

        let mut state = self.state.lock().unwrap();
        assert!(state.update_in_progress);
        state.update_in_progress = false;
        self.cond.notify_all();
        state
    }

    fn cfg_std_read(&self, id: &StdCfgReg, ro: &mut ReadOp) {
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
                if self.lintr_support {
                    let state = self.state.lock().unwrap();
                    if let Some((_id, pin)) = state.attached().lintr_cfg() {
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
                if self.lintr_support {
                    let state = self.state.lock().unwrap();
                    let pin_ident = state
                        .attach
                        .as_ref()
                        .and_then(bus::Attachment::lintr_cfg)
                        .map(|(id, _pin)| *id as u8)
                        .unwrap_or(0);
                    ro.write_u8(pin_ident)
                } else {
                    ro.write_u8(0);
                }
            }
            StdCfgReg::Bar(bar) => {
                let state = self.state.lock().unwrap();
                ro.write_u32(state.bars.reg_read(*bar))
            }
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
                let mut val = HEADER_TYPE_DEVICE;
                let state = self.state.lock().unwrap();
                if state
                    .attach
                    .as_ref()
                    .map(bus::Attachment::is_multifunc)
                    .unwrap_or(false)
                {
                    val |= HEADER_TYPE_MULTIFUNC;
                }
                ro.write_u8(val);
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
    fn cfg_std_write(
        &self,
        dev: &dyn Device,
        id: &StdCfgReg,
        wo: &mut WriteOp,
    ) {
        assert!(wo.offset() == 0 || *id == StdCfgReg::Reserved);

        match id {
            StdCfgReg::Command => {
                let new = RegCmd::from_bits_truncate(wo.read_u16());
                self.reg_cmd_write(dev, new);
            }
            StdCfgReg::IntrLine => {
                self.state.lock().unwrap().reg_intr_line = wo.read_u8();
            }
            StdCfgReg::Bar(bar) => {
                let val = wo.read_u32();
                let mut state = self.state.lock().unwrap();
                if let Some((def, _old, new)) = state.bars.reg_write(*bar, val)
                {
                    let pio_en = state.reg_command.contains(RegCmd::IO_EN);
                    let mmio_en = state.reg_command.contains(RegCmd::MMIO_EN);

                    let attach = state.attached();
                    if (pio_en && def.is_pio()) || (mmio_en && def.is_mmio()) {
                        attach.bar_unregister(*bar);
                        attach.bar_register(*bar, def, new);
                    }
                }
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
    fn reg_cmd_write(&self, dev: &dyn Device, val: RegCmd) {
        let mut state = self.state.lock().unwrap();
        let attach = state.attached();
        let diff = val ^ state.reg_command;

        // Update BAR registrations
        if diff.intersects(RegCmd::IO_EN | RegCmd::MMIO_EN) {
            for n in BarN::iter() {
                let bar = state.bars.get(n);
                if bar.is_none() {
                    continue;
                }
                let (def, v) = bar.unwrap();

                if diff.contains(RegCmd::IO_EN) && def.is_pio() {
                    if val.contains(RegCmd::IO_EN) {
                        attach.bar_register(n, def, v);
                    } else {
                        attach.bar_unregister(n);
                    }
                }
                if diff.contains(RegCmd::MMIO_EN) && def.is_mmio() {
                    if val.contains(RegCmd::MMIO_EN) {
                        attach.bar_register(n, def, v);
                    } else {
                        attach.bar_unregister(n);
                    }
                }
            }
        }

        if diff.intersects(RegCmd::INTX_DIS) {
            // special handling required for INTx enable/disable
            let _state = self.affects_intr_mode(dev, state, |state| {
                state.reg_command = val;
            });
        } else {
            state.reg_command = val;
        }
        // TODO: disable memory and MSI access when busmastering is disabled
    }

    fn which_intr_mode(&self, state: &State) -> IntrMode {
        if self.msix_cfg.is_some()
            && self.msix_cfg.as_ref().unwrap().is_enabled()
        {
            return IntrMode::Msix;
        }
        if let Some(attach) = state.attach.as_ref() {
            if attach.lintr_cfg().is_some()
                && !state.reg_command.contains(RegCmd::INTX_DIS)
            {
                return IntrMode::INTxPin;
            }
        }

        IntrMode::Disabled
    }

    pub(crate) fn get_intr_mode(&self) -> IntrMode {
        let state = self.state.lock().unwrap();
        self.which_intr_mode(&state)
    }

    fn cfg_cap_rw(&self, dev: &dyn Device, id: &CfgReg, rwo: RWOp) {
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
            CfgReg::CapBody(i) => self.do_cap_rw(dev, *i, rwo),

            // Should be filtered down to only cap regs by now
            _ => panic!(),
        }
    }
    fn do_cap_rw(&self, dev: &dyn Device, idx: u8, rwo: RWOp) {
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
                    let _state = self.affects_intr_mode(dev, state, |_state| {
                        msix_cfg.cfg_rw(rwo, |info| {
                            self.notify_msi_update(dev, info)
                        });
                    });
                } else {
                    msix_cfg
                        .cfg_rw(rwo, |info| self.notify_msi_update(dev, info));
                }
            }
            _ => {
                // XXX: do some logging?
            }
        }
    }
    fn notify_msi_update(&self, dev: &dyn Device, info: MsiUpdate) {
        dev.msi_update(info);
    }
    pub fn reset(&self, dev: &dyn Device) {
        let state = self.state.lock().unwrap();

        let mut state = self.affects_intr_mode(dev, state, |state| {
            state.reg_command.reset();
            if let Some(msix) = &self.msix_cfg {
                msix.reset();
            }
        });

        // Both IO and MMIO BARs should be disabled at this point
        debug_assert!(!state
            .reg_command
            .intersects(RegCmd::IO_EN | RegCmd::MMIO_EN));
        for n in BarN::iter() {
            if let Some(_) = state.bars.get(n) {
                state.bars.set(n, 0);
                let attach = state.attached();
                attach.bar_unregister(n);
                // TODO: notify device of zeroed BARs
            }
        }
    }
    fn attach(&self, attachment: bus::Attachment) {
        let mut state = self.state.lock().unwrap();
        let _old = state.attach.replace(attachment);
        assert!(_old.is_none());
        let attach = state.attach.as_ref().unwrap();
        attach.acc_mem.adopt(&self.acc_mem, None);
        attach.acc_msi.adopt(&self.acc_msi, None);
    }

    pub fn lintr_pin(&self) -> Option<Arc<dyn IntrPin>> {
        let state = self.state.lock().unwrap();
        let attach = state.attach.as_ref()?;
        let (_id, pin) = attach.lintr_cfg()?;
        Some(Arc::clone(pin))
    }

    pub fn msix_hdl(&self) -> Option<MsixHdl> {
        let cfg = self.msix_cfg.as_ref()?;
        Some(MsixHdl::new(cfg))
    }

    pub fn export(&self) -> migrate::PciStateV1 {
        let state = self.state.lock().unwrap();
        let msix = self.msix_cfg.as_ref().map(|cfg| cfg.export());
        migrate::PciStateV1 {
            reg_command: state.reg_command.bits(),
            reg_intr_line: state.reg_intr_line,
            bars: state.bars.export(),
            msix,
        }
    }

    pub fn import(
        &self,
        state: migrate::PciStateV1,
    ) -> Result<(), MigrateStateError> {
        let mut inner = self.state.lock().unwrap();
        inner.reg_command =
            RegCmd::from_bits(state.reg_command).ok_or_else(|| {
                MigrateStateError::ImportFailed(format!(
                    "PciState reg_command: failed to import saved value {:#x}",
                    state.reg_command
                ))
            })?;
        inner.reg_intr_line = state.reg_intr_line;
        inner.bars.import(state.bars)?;

        // Reattach any imported Bars to their respective handlers (pio, mmio)
        let attach = inner.attached();
        for n in BarN::iter() {
            if let Some((def, addr)) = inner.bars.get(n) {
                let pio_en = inner.reg_command.contains(RegCmd::IO_EN);
                let mmio_en = inner.reg_command.contains(RegCmd::MMIO_EN);

                if (pio_en && def.is_pio()) || (mmio_en && def.is_mmio()) {
                    attach.bar_register(n, def, addr);
                }
            }
        }

        match (self.msix_cfg.as_ref(), state.msix) {
            (Some(msix_cfg), Some(saved_cfg)) => msix_cfg.import(saved_cfg)?,
            (None, None) => {}
            (None, Some(_)) => {
                return Err(MigrateStateError::ImportFailed(
                    "PciState: device has no MSI-X config".to_string(),
                ))
            }
            (Some(_), None) => {
                return Err(MigrateStateError::ImportFailed(
                    "PciState: device has MSI-X config but none in payload"
                        .to_string(),
                ))
            }
        }

        Ok(())
    }
}

impl MigrateMulti for DeviceState {
    fn export(
        &self,
        output: &mut PayloadOutputs,
        _ctx: &MigrateCtx,
    ) -> Result<(), MigrateStateError> {
        output.push(self.export().into())
    }

    fn import(
        &self,
        offer: &mut PayloadOffers,
        _ctx: &MigrateCtx,
    ) -> Result<(), MigrateStateError> {
        self.import(offer.take()?)
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

#[derive(Debug)]
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

#[derive(Debug, Default)]
struct MsixEntry {
    addr: u64,
    data: u32,
    mask_vec: bool,
    mask_func: bool,
    enabled: bool,
    pending: bool,
    acc_msi: Option<MsiAccessor>,
}
impl MsixEntry {
    fn fire(&mut self) {
        if !self.enabled {
            return;
        }
        if self.mask_func || self.mask_vec {
            self.pending = true;
            return;
        }
        self.send();
    }
    fn check_mask(&mut self) {
        if !self.mask_vec && !self.mask_func && self.pending {
            self.pending = false;
            self.send();
        }
    }
    fn send(&self) {
        if let Some(acc) = self.acc_msi.as_ref() {
            let _ = acc.send(self.addr, self.data as u64);
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

#[derive(Debug)]
struct MsixCfg {
    count: u16,
    bar: BarN,
    pba_off: u32,
    map: RegMap<MsixBarReg>,
    entries: Vec<Mutex<MsixEntry>>,
    state: Mutex<MsixCfgState>,
}
#[derive(Debug, Default)]
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
    fn bar_rw(&self, mut rwo: RWOp, updatef: impl Fn(MsiUpdate)) {
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
                        ent.check_mask();
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
    fn cfg_rw(&self, mut rwo: RWOp, updatef: impl Fn(MsiUpdate)) {
        CAP_MSIX_MAP.process(&mut rwo, |id, rwo| {
            match rwo {
                RWOp::Read(ro) => {
                    match id {
                        MsixCapReg::MsgCtrl => {
                            let state = self.state.lock().unwrap();
                            // low 10 bits hold `count - 1`
                            let mut val = self.count - 1;
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
                                    ent.check_mask();
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
    fn fire(&self, idx: u16) {
        assert!(idx < self.count);
        let mut ent = self.entries[idx as usize].lock().unwrap();
        ent.fire();
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
    fn attach(&self, msi_acc: &MsiAccessor) {
        for entry in self.entries.iter() {
            let mut guard = entry.lock().unwrap();
            guard.acc_msi = Some(msi_acc.child(None));
        }
    }
    fn export(&self) -> migrate::MsixStateV1 {
        let state = self.state.lock().unwrap();
        let mut entries = Vec::new();
        for entry in self.entries.iter() {
            let lentry = entry.lock().unwrap();
            entries.push(migrate::MsixEntryV1 {
                addr: lentry.addr,
                data: lentry.data,
                is_vec_masked: lentry.mask_vec,
                is_pending: lentry.pending,
            });
        }
        migrate::MsixStateV1 {
            count: self.count,
            is_enabled: state.enabled,
            is_func_masked: state.func_mask,
            entries,
        }
    }
    fn import(
        &self,
        state: migrate::MsixStateV1,
    ) -> Result<(), MigrateStateError> {
        let mut inner = self.state.lock().unwrap();

        if self.count != state.count {
            return Err(MigrateStateError::ImportFailed(format!(
                "MsixCfg: count mismatch {} vs {}",
                self.count, state.count
            )));
        }
        if self.entries.len() != state.entries.len() {
            return Err(MigrateStateError::ImportFailed(format!(
                "MsixCfg: entry count mismatch {} vs {}",
                self.entries.len(),
                state.entries.len()
            )));
        }
        inner.enabled = state.is_enabled;
        inner.func_mask = state.is_func_masked;
        for (entry, saved) in self.entries.iter().zip(state.entries) {
            let mut entry = entry.lock().unwrap();
            entry.addr = saved.addr;
            entry.data = saved.data;
            entry.mask_vec = saved.is_vec_masked;
            entry.mask_func = state.is_func_masked;
            entry.enabled = state.is_enabled;
            entry.pending = saved.is_pending;
        }

        Ok(())
    }
}

// public struct for exposing MSI(-X) values
pub struct MsiEnt {
    pub addr: u64,
    pub data: u32,
    pub masked: bool,
    pub pending: bool,
}

#[derive(Debug)]
pub struct MsixHdl {
    cfg: Arc<MsixCfg>,
}
impl MsixHdl {
    fn new(cfg: &Arc<MsixCfg>) -> Self {
        Self { cfg: Arc::clone(cfg) }
    }
    #[cfg(test)]
    pub(crate) fn new_test() -> Self {
        Self { cfg: MsixCfg::new(2048, BarN::BAR0).0 }
    }
    pub fn fire(&self, idx: u16) {
        self.cfg.fire(idx);
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

pub struct Builder {
    ident: Ident,
    lintr_support: bool,
    msix_cfg: Option<Arc<MsixCfg>>,
    bars: [Option<BarDefine>; 6],
    cfg_builder: CfgBuilder,
}

impl Builder {
    pub fn new(ident: Ident) -> Self {
        let mut cfgmap = RegMap::new(LEN_CFG_ECAM);
        cfgmap.define_with_flags(0, LEN_CFG_STD, CfgReg::Std, Flags::PASSTHRU);
        Self {
            ident,
            lintr_support: false,
            msix_cfg: None,
            bars: [None; 6],
            cfg_builder: CfgBuilder::new(),
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
        // TODO: prevent later BAR definition from occupying high word
        self
    }

    /// Add a legacy (pin-based) interrupt
    pub fn add_lintr(mut self) -> Self {
        self.lintr_support = true;
        self
    }

    /// Add a region of the PCI config space for the device which has custom
    /// handling.
    pub fn add_custom_cfg(mut self, offset: u8, len: u8) -> Self {
        self.cfg_builder.add_custom(offset, len);
        self
    }

    fn add_cap_raw(&mut self, id: u8, len: u8) {
        self.cfg_builder.add_capability(id, len);
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

    pub fn finish(self) -> DeviceState {
        let (cfgmap, caps) = self.cfg_builder.finish();
        DeviceState::new(
            self.ident,
            self.lintr_support,
            cfgmap,
            self.msix_cfg,
            caps,
            Bars::new(&self.bars),
        )
    }
}

pub mod migrate {
    use crate::hw::pci::bar;
    use crate::migrate::*;

    use serde::{Deserialize, Serialize};

    #[derive(Deserialize, Serialize)]
    pub struct MsixEntryV1 {
        pub addr: u64,
        pub data: u32,
        pub is_vec_masked: bool,
        pub is_pending: bool,
    }

    #[derive(Deserialize, Serialize)]
    pub struct MsixStateV1 {
        pub count: u16,
        pub is_enabled: bool,
        pub is_func_masked: bool,
        pub entries: Vec<MsixEntryV1>,
    }

    #[derive(Deserialize, Serialize)]
    pub struct PciStateV1 {
        pub reg_command: u16,
        pub reg_intr_line: u8,
        pub bars: bar::migrate::BarStateV1,
        pub msix: Option<MsixStateV1>,
    }
    impl Schema<'_> for PciStateV1 {
        fn id() -> SchemaId {
            ("pci-device", 1)
        }
    }
}

#[cfg(test)]
pub(crate) mod test {
    use super::*;

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

    /// For a given [Device], perform reads of the entire PCI cfg space, 4-bytes
    /// at a time.
    pub(crate) fn cfg_read(dev: &dyn Endpoint) {
        // Read the whole config space for the device, 4 bytes at a time
        let mut buf = [0u8; 4];

        for off in (0..=255).step_by(4) {
            let mut op = ReadOp::from_buf(off, &mut buf[..]);
            dev.cfg_rw(RWOp::Read(&mut op));
        }
    }

    /// For a given [Device], perform writes (of all-1s) of the entire PCI cfg
    /// space, 4-bytes at a time.
    ///
    /// The device is not expected to function well in the face of such abusive
    /// writes, but it should not blow any assertions.
    pub(crate) fn cfg_write(dev: &dyn Endpoint) {
        // Read the whole config space for the device, 4 bytes at a time
        let buf = [0xffu8; 4];

        for off in (0..=255).step_by(4) {
            let mut op = WriteOp::from_buf(off, &buf[..]);
            dev.cfg_rw(RWOp::Write(&mut op));
        }
    }
}
