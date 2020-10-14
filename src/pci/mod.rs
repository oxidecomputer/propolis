use std::sync::{Arc, Mutex, Weak};

use crate::common::*;
use crate::dispatch::DispCtx;
use crate::intr_pins::{IsaPIC, IsaPin};
use crate::pio::PioDev;

use byteorder::{ByteOrder, LE};

mod bits;
mod device;

pub use device::*;

pub const PORT_PCI_CONFIG_ADDR: u16 = 0xcf8;
pub const PORT_PCI_CONFIG_DATA: u16 = 0xcfc;

const MASK_FUNC: u8 = 0x07;
const MASK_DEV: u8 = 0x1f;
const MASK_BUS: u8 = 0xff;

#[derive(Copy, Clone, Eq, PartialEq, Debug)]
pub struct PciBDF {
    bus: u8,
    dev: u8,
    func: u8,
}

#[repr(u8)]
#[derive(Copy, Clone)]
pub enum INTxPin {
    INTA = 1,
    INTB = 2,
    INTC = 3,
    INTD = 4,
}

impl PciBDF {
    pub fn new(bus: u8, dev: u8, func: u8) -> Self {
        assert!(dev <= MASK_DEV);
        assert!(func <= MASK_FUNC);

        Self { bus, dev, func }
    }
}

pub trait PciEndpoint: Send + Sync {
    fn cfg_rw(&self, op: &mut RWOp<'_, '_>, ctx: &DispCtx);
    fn attach(&self, get_lintr: &dyn Fn() -> (INTxPin, IsaPin));
    fn place_bars(
        &self,
        place_bar: &mut dyn FnMut(BarN, &BarDefine) -> u64,
        ctx: &DispCtx,
    );
}

pub struct PciBus {
    state: Mutex<PciBusState>,
    pic: Weak<IsaPIC>,
}

struct PciBusState {
    pio_cfg_addr: u32,
    devices: Vec<(PciBDF, Arc<dyn PciEndpoint>)>,
}

impl PciBusState {
    fn cfg_read(&self, bdf: &PciBDF, ro: &mut ReadOp, ctx: &DispCtx) {
        if let Some((_, dev)) =
            self.devices.iter().find(|(sbdf, _)| sbdf == bdf)
        {
            dev.cfg_rw(&mut RWOp::Read(ro), ctx);
            println!(
                "cfgread bus:{} device:{} func:{} off:{:x}, data:{:x?}",
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
    fn cfg_write(&self, bdf: &PciBDF, wo: &WriteOp, ctx: &DispCtx) {
        if let Some((_, dev)) =
            self.devices.iter().find(|(sbdf, _)| sbdf == bdf)
        {
            println!(
                "cfgwrite bus:{} device:{} func:{} off:{:x}, data:{:x?}",
                bdf.bus, bdf.dev, bdf.func, wo.offset, wo.buf
            );
            dev.cfg_rw(&mut RWOp::Write(wo), ctx);
        } else {
            println!(
                "unhandled cfgwrite bus:{} device:{} func:{} off:{:x}, data:{:x?}",
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
    pub fn new(pic: Weak<IsaPIC>) -> Self {
        Self {
            state: Mutex::new(PciBusState {
                pio_cfg_addr: 0,
                devices: Vec::new(),
            }),
            pic,
        }
    }

    fn route_lintr(&self, bdf: &PciBDF) -> (INTxPin, IsaPin) {
        let pic = Weak::upgrade(&self.pic).unwrap();
        let intx_pin = match (bdf.func + 1) % 4 {
            1 => INTxPin::INTA,
            2 => INTxPin::INTB,
            3 => INTxPin::INTC,
            4 => INTxPin::INTD,
            _ => panic!(),
        };
        // Existing c-bhyve formula: 16 + (4 + slot + INTxPin) % 8
        let pin_route = 16 + ((4 + bdf.dev + intx_pin as u8) % 8);
        let pin = pic.pin_handle(pin_route).unwrap();
        (intx_pin, pin)
    }

    pub fn attach(&self, bdf: PciBDF, dev: Arc<dyn PciEndpoint>) {
        let mut hdl = self.state.lock().unwrap();
        hdl.register(bdf, dev.clone());
        let get_lintr = || self.route_lintr(&bdf);
        dev.attach(&get_lintr);
    }

    pub fn place_bars(&self, ctx: &DispCtx) {
        let mut state = self.state.lock().unwrap();
        // XXX: hack up positioning for now
        let mut pio_pos = 0xc000;
        for (bdf, dev) in state.devices.iter() {
            dev.place_bars(
                &mut |bar, def| match def {
                    BarDefine::Pio(sz) => {
                        let mask = *sz as u64 - 1;
                        let aligned = pio_pos + mask & !mask;
                        pio_pos = aligned + *sz as u64;
                        println!("placing {:?} {:?} @ {:x}", bdf, bar, aligned);
                        aligned
                    }
                    _ => todo!("wire up MMIO later"),
                },
                ctx,
            );
        }
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
    fn pio_out(&self, port: u16, _ident: usize, wo: &WriteOp, ctx: &DispCtx) {
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
                        ctx,
                    );
                }
            }
            _ => {
                panic!();
            }
        }
    }
    fn pio_in(&self, port: u16, _ident: usize, ro: &mut ReadOp, ctx: &DispCtx) {
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
                        ctx,
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
