use std::sync::{Arc, Mutex};

use crate::common::*;
use crate::dispatch::DispCtx;
use crate::util::aspace::ASpace;
pub use crate::util::aspace::{Error, Result};

use byteorder::{ByteOrder, LE};

#[usdt::provider(provider = "propolis")]
mod probes {
    fn mmio_read(addr: u64, bytes: u8, value: u64, was_handled: u8) {}
    fn mmio_write(addr: u64, bytes: u8, value: u64, was_handled: u8) {}
}

pub type MmioFn = dyn Fn(usize, RWOp, &DispCtx) + Send + Sync + 'static;

pub struct MmioBus {
    map: Mutex<ASpace<Arc<MmioFn>>>,
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
        func: Arc<MmioFn>,
    ) -> Result<()> {
        self.map.lock().unwrap().register(start, len, func)
    }
    pub fn unregister(&self, addr: usize) -> Result<()> {
        self.map.lock().unwrap().unregister(addr).map(|_| ())
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
        let handled = self.do_mmio(addr, |a, o, func| {
            let mut wo = WriteOp::from_buf(o as usize, data);
            func(a, RWOp::Write(&mut wo), ctx)
        });
        if !handled {
            slog::info!(ctx.log, "unhandled MMIO";
                "op" => "write", "addr" => addr, "bytes" => bytes);
        }
        probes::mmio_write!(|| (addr as u64, bytes, val, handled as u8));
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
        let handled = self.do_mmio(addr, |a, o, func| {
            let mut ro = ReadOp::from_buf(o as usize, &mut data);
            func(a, RWOp::Read(&mut ro), ctx)
        });
        if !handled {
            slog::info!(ctx.log, "unhandled MMIO";
                "op" => "read", "addr" => addr, "bytes" => bytes);
        }

        let val = LE::read_u64(&buf);
        probes::mmio_read!(|| (addr as u64, bytes, val, handled as u8));
        val
    }

    fn do_mmio<F>(&self, addr: usize, f: F) -> bool
    where
        F: FnOnce(usize, usize, &Arc<MmioFn>),
    {
        let map = self.map.lock().unwrap();
        if let Ok((start, _len, func)) = map.region_at(addr) {
            let func = Arc::clone(func);
            // unlock map before entering handler
            drop(map);
            f(start, addr - start, &func);
            true
        } else {
            false
        }
    }
}
