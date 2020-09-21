use aspace::ASpace;
use std::sync::Arc;

pub trait InoutDev {
    fn pio_out(&self, off: u16, data: &[u8]);
    fn pio_in(&self, off: u16, data: &mut [u8]);
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

    pub fn register(&mut self, start: u16, end: u16, dev: InoutDevHdl) {
        self.map
            .register(start as usize, end as usize, dev)
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
        if let Some((off, dev)) = self.lookup(port) {
            dev.pio_out(off, data);
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
        if let Some((off, dev)) = self.lookup(port) {
            dev.pio_in(off, data);
        } else {
            println!("unhandled IO out - port:{:x} len:{}", port, bytes);
        }

        u32::from_le_bytes(buf)
    }

    fn lookup(&self, port: u16) -> Option<(u16, &InoutDevHdl)> {
        match self.map.region_at(port as usize) {
            Ok((start, _end, hdl)) => Some((port - start as u16, hdl)),
            _ => None,
        }
    }
}
