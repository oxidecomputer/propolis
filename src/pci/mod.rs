use std::sync::{Arc, Mutex};

use crate::pio::PioDev;
use crate::types::*;

use byteorder::{ByteOrder, LE};

mod bits;
mod device;

pub use device::*;

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
        assert!(!self.devices.iter().any(|(sbdf, _)| sbdf == &bdf));
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
