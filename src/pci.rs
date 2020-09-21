use std::convert::TryInto;
use std::sync::Mutex;

use crate::inout::InoutDev;

pub const PORT_PCI_CONFIG_ADDR: u16 = 0xcf8;
pub const PORT_PCI_CONFIG_DATA: u16 = 0xcfc;

pub struct PciBus {
    state: Mutex<PciBusState>,
}

struct PciBusState {
    pio_cfg_addr: u32,
}

impl PciBus {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(PciBusState { pio_cfg_addr: 0 }),
        }
    }
}

fn read_inval(data: &mut [u8]) {
    for b in data.iter_mut() {
        *b = 0xffu8;
    }
}

fn do_cfg_read(addr: u32) {
    let offset = addr & 0xff;
    let func = (addr & 0x700) >> 8;
    let device = (addr & 0xf800) >> 11;
    let bus = (addr & 0xff0000) >> 16;
    println!(
        "cfgread bus:{} device:{} func:{} off:{:x}",
        bus, device, func, offset
    );
}

impl InoutDev for PciBus {
    fn pio_out(&self, port: u16, off: u16, data: &[u8]) {
        if off != 0 || data.len() != 4 {
            // demand aligned/sized access
            return;
        }
        let mut hdl = self.state.lock().unwrap();
        match port {
            PORT_PCI_CONFIG_ADDR => {
                hdl.pio_cfg_addr = u32::from_le_bytes(data.try_into().unwrap());
            }
            PORT_PCI_CONFIG_DATA => {
                // ignore writes
            }
            _ => {
                panic!();
            }
        }
    }
    fn pio_in(&self, port: u16, off: u16, data: &mut [u8]) {
        if off != 0 {
            // demand aligned/sized access
            read_inval(data);
            return;
        }
        let hdl = self.state.lock().unwrap();
        match port {
            PORT_PCI_CONFIG_ADDR => {
                let buf = u32::to_le_bytes(hdl.pio_cfg_addr);
                data.copy_from_slice(&buf);
            }
            PORT_PCI_CONFIG_DATA => {
                do_cfg_read(hdl.pio_cfg_addr);
                // assume no devices for now
                read_inval(data);
            }
            _ => {
                panic!();
            }
        }
    }
}
