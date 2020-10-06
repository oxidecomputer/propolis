use std::sync::{Arc, Mutex};

use crate::pio::PioDev;
use crate::types::*;
use crate::util::regmap::{Flags, RegMap};

use byteorder::{ByteOrder, LE};

mod bits;
use bits::*;

pub const PORT_PCI_CONFIG_ADDR: u16 = 0xcf8;
pub const PORT_PCI_CONFIG_DATA: u16 = 0xcfc;

const MASK_FUNC: u8 = 0x07;
const MASK_DEV: u8 = 0x1f;
const MASK_BUS: u8 = 0xff;

#[derive(Copy, Clone, Eq, PartialEq)]
pub struct PciBDF {
    bus: u8,
    dev: u8,
    func: u8,
}

impl PciBDF {
    pub fn new(bus: u8, dev: u8, func: u8) -> Self {
        assert!(dev <= MASK_DEV);
        assert!(func <= MASK_FUNC);

        Self { bus, dev, func }
    }
}

pub trait PciEndpoint: Send + Sync {
    fn cfg_read(&self, ro: &mut ReadOp);
    fn cfg_write(&self, wo: &WriteOp);
}

#[derive(Copy, Clone, Eq, PartialEq, Debug)]
enum PciCfgReg {
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

#[derive(Copy, Clone, Eq, PartialEq, Debug)]
pub enum BarN {
    BAR0,
    BAR1,
    BAR2,
    BAR3,
    BAR4,
    BAR5,
}

// register layout info
static CFG_LAYOUT: [(PciCfgReg, u8, u8); 28] = [
    (PciCfgReg::VendorId, OFF_CFG_VENDORID, 2),
    (PciCfgReg::DeviceId, OFF_CFG_DEVICEID, 2),
    (PciCfgReg::Command, OFF_CFG_COMMAND, 2),
    (PciCfgReg::Status, OFF_CFG_STATUS, 2),
    (PciCfgReg::RevisionId, OFF_CFG_REVISIONID, 1),
    (PciCfgReg::ProgIf, OFF_CFG_PROGIF, 1),
    (PciCfgReg::Subclass, OFF_CFG_SUBCLASS, 1),
    (PciCfgReg::Class, OFF_CFG_CLASS, 1),
    (PciCfgReg::CacheLineSize, OFF_CFG_CACHELINESZ, 1),
    (PciCfgReg::LatencyTimer, OFF_CFG_LATENCYTIMER, 1),
    (PciCfgReg::HeaderType, OFF_CFG_HEADERTYPE, 1),
    (PciCfgReg::Bist, OFF_CFG_BIST, 1),
    (PciCfgReg::Bar(BarN::BAR0), OFF_CFG_BAR0, 4),
    (PciCfgReg::Bar(BarN::BAR1), OFF_CFG_BAR1, 4),
    (PciCfgReg::Bar(BarN::BAR2), OFF_CFG_BAR2, 4),
    (PciCfgReg::Bar(BarN::BAR3), OFF_CFG_BAR3, 4),
    (PciCfgReg::Bar(BarN::BAR4), OFF_CFG_BAR4, 4),
    (PciCfgReg::Bar(BarN::BAR5), OFF_CFG_BAR5, 4),
    (PciCfgReg::CardbusPtr, OFF_CFG_CARDBUSPTR, 4),
    (PciCfgReg::SubVendorId, OFF_CFG_SUBVENDORID, 2),
    (PciCfgReg::SubDeviceId, OFF_CFG_SUBDEVICEID, 2),
    (PciCfgReg::ExpansionRomAddr, OFF_CFG_EXPROMADDR, 4),
    (PciCfgReg::CapPtr, OFF_CFG_CAPPTR, 1),
    // Reserved bytes between CapPtr and IntrLine [0x35-0x3c)
    (
        PciCfgReg::Reserved,
        OFF_CFG_RESERVED,
        OFF_CFG_INTRLINE - OFF_CFG_RESERVED,
    ),
    (PciCfgReg::IntrLine, OFF_CFG_INTRLINE, 1),
    (PciCfgReg::IntrPin, OFF_CFG_INTRPIN, 1),
    (PciCfgReg::MinGrant, OFF_CFG_MINGRANT, 1),
    (PciCfgReg::MaxLatency, OFF_CFG_MAXLATENCY, 1),
];

fn pci_cfg_regmap(map: &mut RegMap<PciCfgReg>) {
    for reg in CFG_LAYOUT.iter() {
        if reg.0 != PciCfgReg::Reserved {
            map.define(reg.1 as usize, reg.2 as usize, reg.0)
        } else {
            // The reserved section is empty, so the register does not need a buffer padded to its
            // own size for reads or writes.
            map.define_with_flags(
                reg.1 as usize,
                reg.2 as usize,
                reg.0,
                Flags::NO_READ_EXTEND | Flags::NO_WRITE_EXTEND,
            );
        }
    }
}

#[derive(Default)]
struct PciState {
    command: u16,
    intr_line: u8,
    intr_pin: u8,
}

pub struct PciDevInst<I: Send> {
    vendor_id: u16,
    device_id: u16,
    class: u8,
    subclass: u8,
    header: Mutex<PciState>,
    regmap: RegMap<PciCfgReg>,
    devimpl: I,
}

impl<I: Send> PciDevInst<I> {
    pub fn new(
        vendor_id: u16,
        device_id: u16,
        class: u8,
        subclass: u8,
        i: I,
    ) -> Self {
        let mut regmap = RegMap::new(0x40);
        pci_cfg_regmap(&mut regmap);

        Self {
            vendor_id,
            device_id,
            class,
            subclass,
            header: Mutex::new(PciState {
                command: 0x0000,
                intr_line: 0xff,
                intr_pin: 0x00,
            }),
            regmap,
            devimpl: i,
        }
    }

    fn cfg_std_read(&self, id: &PciCfgReg, ro: &mut ReadOp) {
        // Only the reserved space is configured to skip buffering for unaligned access.
        assert!(ro.offset == 0 || *id == PciCfgReg::Reserved);

        let outb = &mut ro.buf;
        match id {
            PciCfgReg::VendorId => LE::write_u16(outb, self.vendor_id),
            PciCfgReg::DeviceId => LE::write_u16(outb, self.device_id),
            PciCfgReg::Class => outb[0] = self.class,
            PciCfgReg::Subclass => outb[0] = self.subclass,

            PciCfgReg::Command => {
                let state = self.header.lock().unwrap();
                LE::write_u16(outb, state.command);
            }
            PciCfgReg::IntrLine => {
                let state = self.header.lock().unwrap();
                outb[0] = state.intr_line;
            }
            PciCfgReg::IntrPin => {
                let state = self.header.lock().unwrap();
                outb[0] = state.intr_pin;
            }
            PciCfgReg::Reserved => {
                outb.iter_mut().for_each(|b| *b = 0);
            }
            _ => {
                println!("Unhandled read {:?}", id);
                outb.iter_mut().for_each(|b| *b = 0);
            }
        }
    }

    fn cfg_std_write(&self, id: &PciCfgReg, wo: &WriteOp) {
        // Only the reserved space is configured to skip buffering for unaligned access.
        assert!(wo.offset == 0 || *id == PciCfgReg::Reserved);

        let inb = wo.buf;
        let mut state = self.header.lock().unwrap();
        match id {
            PciCfgReg::Command => {
                let new = LE::read_u16(inb);
                // mask all bits but io/mmio/busmaster enable and INTx disable
                state.command = new & REG_MASK_CMD
            }
            PciCfgReg::IntrLine => {
                state.intr_line = inb[0];
            }
            PciCfgReg::Reserved => {}
            _ => {
                println!("Unhandled write {:?}", id);
                // discard all other writes
            }
        }
    }
}

impl<I: Send + Sync> PciEndpoint for PciDevInst<I> {
    fn cfg_read(&self, ro: &mut ReadOp) {
        self.regmap
            .with_ctx(self, Self::cfg_std_read, Self::cfg_std_write)
            .read(ro);
    }

    fn cfg_write(&self, wo: &WriteOp) {
        self.regmap
            .with_ctx(self, Self::cfg_std_read, Self::cfg_std_write)
            .write(wo);
    }
}

pub trait DevImpl {
    fn bar_read(&self, bar: BarN, offset: usize, outb: &mut [u8]) {
        panic!("unexpected BAR read: {:?} @ {} {}", bar, offset, outb.len());
    }
    fn bar_write(&self, bar: BarN, offset: usize, inb: &[u8]) {
        panic!("unexpected BAR write: {:?} @ {} {}", bar, offset, inb.len());
    }
    // TODO
    // fn cap_read(&self);
    // fn cap_write(&self);
}

pub struct PciBus {
    state: Mutex<PciBusState>,
}

struct PciBusState {
    pio_cfg_addr: u32,
    devices: Vec<(PciBDF, Arc<dyn PciEndpoint>)>,
}

impl PciBusState {
    fn cfg_read(&self, bdf: &PciBDF, ro: &mut ReadOp) {
        if let Some((_, dev)) =
            self.devices.iter().find(|(sbdf, _)| sbdf == bdf)
        {
            dev.cfg_read(ro);
            println!(
                "cfgread bus:{} device:{} func:{} off:{:x}, data:{:?}",
                bdf.bus, bdf.dev, bdf.func, ro.offset, ro.buf
            );
        } else {
            println!(
                "unhandled cfgread bus:{} device:{} func:{} off:{:x}",
                bdf.bus, bdf.dev, bdf.func, ro.offset
            );
            read_inval(ro.buf);
        }
    }
    fn cfg_write(&self, bdf: &PciBDF, wo: &WriteOp) {
        if let Some((_, dev)) =
            self.devices.iter().find(|(sbdf, _)| sbdf == bdf)
        {
            println!(
                "cfgwrite bus:{} device:{} func:{} off:{:x}, data:{:?}",
                bdf.bus, bdf.dev, bdf.func, wo.offset, wo.buf
            );
            dev.cfg_write(wo);
        } else {
            println!(
                "unhandled cfgwrite bus:{} device:{} func:{} off:{:x}, data:{:?}",
                bdf.bus, bdf.dev, bdf.func, wo.offset, wo.buf
            );
        }
    }

    fn register(&mut self, bdf: PciBDF, dev: Arc<dyn PciEndpoint>) {
        // XXX strict fail for now
        assert!(!self.devices.iter().find(|(sbdf, _)| sbdf == &bdf).is_some());
        self.devices.push((bdf, dev));
    }
}

impl PciBus {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(PciBusState {
                pio_cfg_addr: 0,
                devices: Vec::new(),
            }),
        }
    }

    pub fn attach(&self, bdf: PciBDF, dev: Arc<dyn PciEndpoint>) {
        let mut hdl = self.state.lock().unwrap();
        hdl.register(bdf, dev);
    }
}

fn read_inval(data: &mut [u8]) {
    for b in data.iter_mut() {
        *b = 0xffu8;
    }
}

fn cfg_addr_parse(addr: u32) -> Option<(PciBDF, u8)> {
    if addr & 0x80000000 == 0 {
        // Enable bit not set
        None
    } else {
        let offset = addr & 0xff;
        let func = (addr >> 8) as u8 & MASK_FUNC;
        let device = (addr >> 11) as u8 & MASK_DEV;
        let bus = (addr >> 16) as u8 & MASK_BUS;

        Some((PciBDF::new(bus, device, func), offset as u8))
    }
}

impl PioDev for PciBus {
    fn pio_out(&self, port: u16, wo: &WriteOp) {
        let mut hdl = self.state.lock().unwrap();
        match port {
            PORT_PCI_CONFIG_ADDR => {
                if wo.buf.len() == 4 && wo.offset == 0 {
                    // XXX expect aligned/sized reads
                    hdl.pio_cfg_addr = LE::read_u32(wo.buf);
                }
            }
            PORT_PCI_CONFIG_DATA => {
                if let Some((bdf, cfg_off)) = cfg_addr_parse(hdl.pio_cfg_addr) {
                    hdl.cfg_write(
                        &bdf,
                        &WriteOp::new(wo.offset + cfg_off as usize, wo.buf),
                    );
                }
            }
            _ => {
                panic!();
            }
        }
    }
    fn pio_in(&self, port: u16, ro: &mut ReadOp) {
        let hdl = self.state.lock().unwrap();
        match port {
            PORT_PCI_CONFIG_ADDR => {
                if ro.buf.len() == 4 && ro.offset == 0 {
                    // XXX expect aligned/sized reads
                    LE::write_u32(ro.buf, hdl.pio_cfg_addr);
                } else {
                    read_inval(ro.buf);
                }
            }
            PORT_PCI_CONFIG_DATA => {
                if let Some((bdf, cfg_off)) = cfg_addr_parse(hdl.pio_cfg_addr) {
                    hdl.cfg_read(
                        &bdf,
                        &mut ReadOp::new(ro.offset + cfg_off as usize, ro.buf),
                    );
                } else {
                    read_inval(ro.buf);
                }
            }
            _ => {
                panic!();
            }
        }
    }
}
