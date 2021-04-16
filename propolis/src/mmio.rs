use std::sync::{Arc, Mutex, Weak};

use crate::common::*;
use crate::dispatch::DispCtx;
use crate::util::aspace::ASpace;
pub use crate::util::aspace::{Error, Result};

use byteorder::{ByteOrder, LE};

pub trait MmioDev: Send + Sync {
    fn mmio_rw(&self, addr: usize, ident: usize, rwop: RWOp, ctx: &DispCtx);
}

pub struct MmioBus {
    map: Mutex<ASpace<(Weak<dyn MmioDev>, usize)>>,
}
impl MmioBus {
    pub fn new(max: usize) -> Self {
        assert!(max != 0);
        Self { map: Mutex::new(ASpace::new(0, max)) }
    }

    pub fn register(
        &self,
        start: usize,
        len: usize,
        dev: Weak<dyn MmioDev>,
        ident: usize,
    ) -> Result<()> {
        self.map.lock().unwrap().register(start, len, (dev, ident))
    }
    pub fn unregister(
        &self,
        addr: usize,
    ) -> Result<(Weak<dyn MmioDev>, usize)> {
        self.map.lock().unwrap().unregister(addr)
    }

    pub fn handle_write(
        &self,
        addr: usize,
        bytes: u8,
        val: u64,
        ctx: &DispCtx,
    ) {
        let buf = val.to_le_bytes();
        let data = match bytes {
            1 => &buf[0..1],
            2 => &buf[0..2],
            4 => &buf[0..4],
            8 => &buf[0..],
            _ => panic!(),
        };
        let handled = self.do_mmio(addr, |a, o, dev, ident| {
            let mut wo = WriteOp::from_buf(o as usize, data);
            dev.mmio_rw(a, ident, RWOp::Write(&mut wo), ctx)
        });
        if !handled {
            println!("unhandled MMIO write - addr:{:x} len:{}", addr, bytes);
        }
        probe_mmio_write!(|| (addr as u64, bytes, val, handled as u8));
    }
    pub fn handle_read(&self, addr: usize, bytes: u8, ctx: &DispCtx) -> u64 {
        let mut buf = [0xffu8; 8];
        let mut data = match bytes {
            1 => &mut buf[0..1],
            2 => &mut buf[0..2],
            4 => &mut buf[0..4],
            8 => &mut buf[0..],
            _ => panic!(),
        };
        let handled = self.do_mmio(addr, |a, o, dev, ident| {
            let mut ro = ReadOp::from_buf(o as usize, &mut data);
            dev.mmio_rw(a, ident, RWOp::Read(&mut ro), ctx)
        });
        if !handled {
            println!("unhandled MMIO read - addr:{:x} len:{}", addr, bytes);
        }

        let val = LE::read_u64(&buf);
        probe_mmio_read!(|| (addr as u64, bytes, val, handled as u8));
        val
    }

    fn do_mmio<F>(&self, addr: usize, f: F) -> bool
    where
        F: FnOnce(usize, usize, &Arc<dyn MmioDev>, usize),
    {
        let map = self.map.lock().unwrap();
        if let Ok((start, _len, (weak, ident))) = map.region_at(addr) {
            let dev = Weak::upgrade(weak).unwrap();
            let identv = *ident;
            // unlock map before entering handler
            drop(map);
            f(start, addr - start, &dev, identv);
            true
        } else {
            false
        }
    }
}
