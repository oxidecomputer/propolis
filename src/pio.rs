use std::sync::{Arc, Mutex, Weak};

use crate::common::*;
use crate::dispatch::DispCtx;
use crate::util::aspace::ASpace;
pub use crate::util::aspace::{Error, Result};

use byteorder::{ByteOrder, LE};

pub trait PioDev: Send + Sync {
    fn pio_rw(&self, port: u16, ident: usize, rwop: &mut RWOp, ctx: &DispCtx);
}

pub struct PioBus {
    map: Mutex<ASpace<(Weak<dyn PioDev>, usize)>>,
}

impl PioBus {
    pub fn new() -> Self {
        Self { map: Mutex::new(ASpace::new(0, u16::MAX as usize)) }
    }

    pub fn register(
        &self,
        start: u16,
        len: u16,
        dev: Weak<dyn PioDev>,
        ident: usize,
    ) -> Result<()> {
        self.map.lock().unwrap().register(
            start as usize,
            len as usize,
            (dev, ident),
        )
    }
    pub fn unregister(&self, start: u16) -> Result<(Weak<dyn PioDev>, usize)> {
        self.map.lock().unwrap().unregister(start as usize)
    }

    pub fn handle_out(&self, port: u16, bytes: u8, val: u32, ctx: &DispCtx) {
        let buf = val.to_le_bytes();
        let data = match bytes {
            1 => &buf[0..1],
            2 => &buf[0..2],
            4 => &buf[0..],
            _ => panic!(),
        };
        let handled = self.do_pio(port, |p, o, dev, ident| {
            let wo = WriteOp::new(o as usize, data);
            dev.pio_rw(p, ident, &mut RWOp::Write(&wo), ctx)
        });
        if !handled {
            println!("unhandled IO out - port:{:x} len:{}", port, bytes);
        }
    }

    pub fn handle_in(&self, port: u16, bytes: u8, ctx: &DispCtx) -> u32 {
        let mut buf = [0xffu8; 4];
        let data = match bytes {
            1 => &mut buf[0..1],
            2 => &mut buf[0..2],
            4 => &mut buf[0..],
            _ => panic!(),
        };
        let handled = self.do_pio(port, |p, o, dev, ident| {
            let mut ro = ReadOp::new(o as usize, data);
            dev.pio_rw(p, ident, &mut RWOp::Read(&mut ro), ctx)
        });
        if !handled {
            println!("unhandled IO in - port:{:x} len:{}", port, bytes);
        }

        LE::read_u32(&buf)
    }

    fn do_pio<F>(&self, port: u16, f: F) -> bool
    where
        F: FnOnce(u16, u16, &Arc<dyn PioDev>, usize),
    {
        let map = self.map.lock().unwrap();
        if let Ok((start, _len, (weak, ident))) = map.region_at(port as usize) {
            let dev = Weak::upgrade(weak).unwrap();
            let identv = *ident;
            // unlock map before entering handler
            drop(map);
            f(start as u16, port - start as u16, &dev, identv);
            true
        } else {
            false
        }
    }
}
