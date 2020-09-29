use std::convert::TryInto;
use std::sync::{Arc, Mutex};

use crate::inout::InoutDev;
use crate::util::regmap::{Flags, RegMap};

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

fn pci_cfg_regmap<CTX>(map: &mut RegMap<PciCfgReg, CTX>) {
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
#[allow(dead_code)]
struct PciState {
    vendor_id: u16,
    device_id: u16,
    class: u8,
    subclass: u8,
    command: u16,
    intr_line: u8,
    intr_pin: u8,
}

impl PciState {
    fn cfg_read(&mut self, id: PciCfgReg, buf: &mut [u8]) {
        match id {
            PciCfgReg::VendorId => buf.copy_from_slice(&u16::to_le_bytes(self.vendor_id)),
            PciCfgReg::DeviceId => buf.copy_from_slice(&u16::to_le_bytes(self.device_id)),
            PciCfgReg::Command => buf.copy_from_slice(&u16::to_le_bytes(self.command)),
            PciCfgReg::Class => buf[0] = self.class,
            PciCfgReg::Subclass => buf[0] = self.subclass,
            PciCfgReg::IntrLine => buf[0] = self.intr_line,
            PciCfgReg::IntrPin => buf[0] = self.intr_pin,
            _ => {
                println!("Unhandled read {:?}", id);
                buf.iter_mut().for_each(|b| *b = 0);
            }
        }
    }

    fn cfg_write(&mut self, id: PciCfgReg, buf: &[u8]) {
        match id {
            PciCfgReg::Command => {
                let new = u16::from_le_bytes(buf.try_into().unwrap());
                // mask all bits but io/mmio/busmaster enable and INTx disable
                self.command = new & REG_MASK_CMD
            }
            PciCfgReg::IntrLine => {
                self.intr_line = buf[0];
            }
            PciCfgReg::IntrPin => {
                self.intr_pin = buf[0];
            }
            _ => {
                println!("Unhandled write {:?}", id);
                // discard all other writes
            }
        }
    }

    fn cfg_partial_read(&mut self, id: PciCfgReg, _off: usize, buf: &mut [u8]) {
        assert!(id == PciCfgReg::Reserved);
        buf.iter_mut().for_each(|b| *b = 0);
    }

    fn cfg_partial_write(&mut self, id: PciCfgReg, _off: usize, _buf: &[u8]) {
        assert!(id == PciCfgReg::Reserved);
    }
}

pub struct PciDev {
    header: Mutex<PciState>,
    regmap: RegMap<PciCfgReg, PciState>,
}
impl PciDev {
    pub fn new(vendor_id: u16, device_id: u16, class: u8, subclass: u8) -> Self {
        let mut regmap = RegMap::new(0x40, PciState::cfg_read, PciState::cfg_write);
        regmap.set_partial_handlers(
            Some(PciState::cfg_partial_read),
            Some(PciState::cfg_partial_write),
        );
        pci_cfg_regmap(&mut regmap);

        Self {
            header: Mutex::new(PciState {
                vendor_id,
                device_id,
                class,
                subclass,
                command: 0x0004, //busmaster enable
                intr_line: 0xff,
                intr_pin: 0x00,
            }),
            regmap,
        }
    }

    fn cfg_read(&self, offset: u8, data: &mut [u8]) {
        let off = offset as usize;

        // XXX be picky for now
        assert!(off + data.len() <= 0x40);
        let mut header = self.header.lock().unwrap();
        self.regmap.read(off, data, &mut header);
    }

    fn cfg_write(&self, offset: u8, data: &[u8]) {
        let off = offset as usize;

        // XXX be picky for now
        assert!(off + data.len() <= 0x40);
        let mut header = self.header.lock().unwrap();
        self.regmap.write(off, data, &mut header);
    }
}

pub struct PciBus {
    state: Mutex<PciBusState>,
}

struct PciBusState {
    pio_cfg_addr: u32,
    devices: Vec<(PciBDF, PciDev)>,
}

impl PciBusState {
    fn cfg_read(&self, bdf: &PciBDF, offset: u8, data: &mut [u8]) {
        if let Some((_, dev)) = self.devices.iter().find(|(sbdf, _)| sbdf == bdf) {
            dev.cfg_read(offset, data);
            println!(
                "cfgread bus:{} device:{} func:{} off:{:x}, data:{:?}",
                bdf.bus, bdf.dev, bdf.func, offset, data
            );
        } else {
            println!(
                "unhandled cfgread bus:{} device:{} func:{} off:{:x}",
                bdf.bus, bdf.dev, bdf.func, offset
            );
            read_inval(data);
        }
    }
    fn cfg_write(&self, bdf: &PciBDF, offset: u8, data: &[u8]) {
        if let Some((_, dev)) = self.devices.iter().find(|(sbdf, _)| sbdf == bdf) {
            println!(
                "cfgwrite bus:{} device:{} func:{} off:{:x}, data:{:?}",
                bdf.bus, bdf.dev, bdf.func, offset, data
            );
            dev.cfg_write(offset, data);
        } else {
            println!(
                "unhandled cfgwrite bus:{} device:{} func:{} off:{:x}, data:{:?}",
                bdf.bus, bdf.dev, bdf.func, offset, data
            );
        }
    }

    fn register(&mut self, bdf: PciBDF, dev: PciDev) {
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

    pub fn register(&self, bdf: PciBDF, dev: PciDev) {
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

impl InoutDev for PciBus {
    fn pio_out(&self, port: u16, off: u16, data: &[u8]) {
        let mut hdl = self.state.lock().unwrap();
        match port {
            PORT_PCI_CONFIG_ADDR => {
                if data.len() == 4 && off == 0 {
                    // XXX expect aligned/sized reads
                    hdl.pio_cfg_addr = u32::from_le_bytes(data.try_into().unwrap());
                }
            }
            PORT_PCI_CONFIG_DATA => {
                if let Some((bdf, cfg_off)) = cfg_addr_parse(hdl.pio_cfg_addr) {
                    hdl.cfg_write(&bdf, cfg_off + off as u8, data);
                }
            }
            _ => {
                panic!();
            }
        }
    }
    fn pio_in(&self, port: u16, off: u16, data: &mut [u8]) {
        let hdl = self.state.lock().unwrap();
        match port {
            PORT_PCI_CONFIG_ADDR => {
                let buf = u32::to_le_bytes(hdl.pio_cfg_addr);
                if data.len() == 4 && off == 0 {
                    // XXX expect aligned/sized reads
                    data.copy_from_slice(&buf);
                } else {
                    read_inval(data);
                }
            }
            PORT_PCI_CONFIG_DATA => {
                if let Some((bdf, cfg_off)) = cfg_addr_parse(hdl.pio_cfg_addr) {
                    hdl.cfg_read(&bdf, cfg_off + off as u8, data);
                } else {
                    read_inval(data);
                }
            }
            _ => {
                panic!();
            }
        }
    }
}
