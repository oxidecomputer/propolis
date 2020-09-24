use crate::util::aspace::ASpace;
use std::sync::Arc;

pub trait InoutDev {
    fn pio_out(&self, port: u16, off: u16, data: &[u8]);
    fn pio_in(&self, port: u16, off: u16, data: &mut [u8]);
}

type InoutDevHdl = Arc<dyn InoutDev>;

pub struct InoutBus {
    map: ASpace<InoutDevHdl>,
}

impl InoutBus {
    pub fn new() -> Self {
        Self {
            map: ASpace::new(0, u16::MAX as usize),
        }
    }

    pub fn register(&mut self, start: u16, len: u16, dev: InoutDevHdl) {
        self.map
            .register(start as usize, len as usize, dev)
            .unwrap();
    }

    pub fn handle_out(&self, port: u16, bytes: u8, val: u32) {
        let buf = val.to_le_bytes();
        let data = match bytes {
            1 => &buf[0..1],
            2 => &buf[0..2],
            4 => &buf[0..],
            _ => panic!(),
        };
        if let Some((port, off, dev)) = self.lookup(port) {
            dev.pio_out(port, off, data);
        } else {
            println!(
                "unhandled IO out - port:{:x} len:{} val:{:x}",
                port, bytes, val
            );
        }
    }

    pub fn handle_in(&self, port: u16, bytes: u8) -> u32 {
        let mut buf = [0xffu8; 4];
        let data = match bytes {
            1 => &mut buf[0..1],
            2 => &mut buf[0..2],
            4 => &mut buf[0..],
            _ => panic!(),
        };
        if let Some((port, off, dev)) = self.lookup(port) {
            dev.pio_in(port, off, data);
        } else {
            println!("unhandled IO in - port:{:x} len:{}", port, bytes);
        }

        u32::from_le_bytes(buf)
    }

    fn lookup(&self, port: u16) -> Option<(u16, u16, &InoutDevHdl)> {
        match self.map.region_at(port as usize) {
            Ok((start, _len, hdl)) => Some((start as u16, port - start as u16, hdl)),
            _ => None,
        }
    }
}
