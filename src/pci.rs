use std::convert::TryInto;
use std::sync::{Arc, Mutex};

use crate::inout::InoutDev;

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

#[repr(packed)]
#[derive(Default)]
struct PciHeader {
    vendor_id: u16,
    device_id: u16,
    command: u16,
    status: u16,
    revision_id: u8,
    prog_if: u8,
    subclass: u8,
    class: u8,
    cache_line_size: u8,
    latency_timer: u8,
    header_type: u8,
    bist: u8,
    bars: [u32; 6],
    cardbus_ptr: u32,
    sub_vendor_id: u16,
    sub_device_id: u16,
    expansion_rom_addr: u32,
    cap_ptr: u8,
    reserved1: [u8; 3],
    reserved2: u32,
    intr_line: u8,
    intr_pin: u8,
    min_grant: u8,
    max_latency: u8,
}

impl PciHeader {
    fn as_bytes(&self) -> &[u8] {
        unsafe {
            // struct is packed, so there should be no UB-inducing padding
            std::slice::from_raw_parts(
                (self as *const Self) as *const u8,
                std::mem::size_of::<Self>(),
            )
        }
    }
}

pub struct PciDev {
    header: PciHeader,
}
impl PciDev {
    pub fn new(vendor_id: u16, device_id: u16, class: u8, subclass: u8) -> Self {
        Self {
            header: PciHeader {
                vendor_id,
                device_id,
                class,
                subclass,
                command: 0x0004, //busmaster enable
                intr_line: 0xff,
                intr_pin: 0x00,
                ..Default::default()
            },
        }
    }

    fn cfg_read(&self, offset: u8, data: &mut [u8]) {
        let cfg = self.header.as_bytes();
        let off = offset as usize;

        if off >= cfg.len() {
            read_inval(data);
            return;
        }

        // XXX be picky for now
        assert!(off + data.len() <= cfg.len());
        let to_copy = &cfg[off..(off + data.len())];
        data.copy_from_slice(to_copy);
    }
    fn cfg_write(&mut self, offset: u8, data: &[u8]) {
        // XXX total hack for now
        if data.len() == 2 && offset == 0x4 {
            let word = u16::from_le_bytes(data[0..=1].try_into().unwrap());
            // allow setting the command field
            self.header.command = word;
        }
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
    fn cfg_write(&mut self, bdf: &PciBDF, offset: u8, data: &[u8]) {
        if let Some((_, dev)) = self.devices.iter_mut().find(|(sbdf, _)| sbdf == bdf) {
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
