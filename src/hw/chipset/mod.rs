use std::sync::Arc;

use crate::dispatch::DispCtx;
use crate::hw::pci::{BarDefine, Endpoint, BDF};

pub mod i440fx;

pub trait Chipset {
    fn pci_attach(&self, bdf: BDF, dev: Arc<dyn Endpoint>);
    fn pci_finalize(&self, ctx: &DispCtx);
}

pub(self) struct BarPlacer<T> {
    pio_bars: Vec<(T, usize)>,
    mmio_bars: Vec<(T, usize)>,

    pio_avail: Option<(usize, usize)>,
    mmio_avail: Option<(usize, usize)>,
}
impl<T: Copy + Sized> BarPlacer<T> {
    pub fn new() -> Self {
        Self {
            pio_bars: Vec::new(),
            pio_avail: None,

            mmio_bars: Vec::new(),
            mmio_avail: None,
        }
    }
    pub fn add_bar(&mut self, loc: T, def: &BarDefine) {
        match def {
            BarDefine::Pio(sz) => {
                self.pio_bars.push((loc, *sz as usize));
            }
            BarDefine::Mmio(sz) => {
                self.mmio_bars.push((loc, *sz as usize));
            }
            _ => todo!("handle 64-bit bars later"),
        }
    }
    pub fn add_avail_pio(&mut self, port: u16, len: u16) {
        assert!(len != 0);
        assert!(port.checked_add(len - 1).is_some());

        // Only allow one region for now
        assert!(self.pio_avail.is_none());
        self.pio_avail = Some((port as usize, len as usize));
    }
    pub fn add_avail_mmio(&mut self, addr: u32, len: u32) {
        assert!(len != 0);
        assert!(addr.checked_add(len - 1).is_some());

        // Only allow one region for now
        assert!(self.mmio_avail.is_none());
        self.mmio_avail = Some((addr as usize, len as usize));
    }
    pub fn place(self, mut cb: impl FnMut(T, usize)) -> Option<(usize, usize)> {
        assert!(self.pio_avail.is_some());
        assert!(self.mmio_avail.is_some());

        let (pio_start, pio_len) = self.pio_avail.unwrap();
        let (mmio_start, mmio_len) = self.mmio_avail.unwrap();

        let pio_remain =
            Self::simple_placement(self.pio_bars, pio_start, pio_len, &mut cb);
        let mmio_remain = Self::simple_placement(
            self.mmio_bars,
            mmio_start,
            mmio_len,
            &mut cb,
        );
        if pio_remain.is_none() && mmio_remain.is_none() {
            None
        } else {
            Some((pio_remain.unwrap_or(0), mmio_remain.unwrap_or(0)))
        }
    }

    fn simple_placement(
        mut bars: Vec<(T, usize)>,
        avail_start: usize,
        avail_len: usize,
        cb: &mut impl FnMut(T, usize),
    ) -> Option<usize> {
        // Strategy: Place BARs from smallest to largest.  This should help
        // fulfill the natural power-of-two alignment requirements in the space
        // available, rather than leaving gaps.
        let mut addr = avail_start;
        let mut remain = avail_len;
        bars.sort_by_key(|x| x.1);
        while let Some((id, sz)) = bars.pop() {
            assert!(sz.is_power_of_two());

            let (aligned, offset) = align_to(addr, sz);
            if remain < offset || (remain - offset) < sz {
                break;
            }
            cb(id, aligned);

            remain -= offset + sz;
            addr += offset + sz;
        }

        if !bars.is_empty() {
            Some(bars.iter().map(|x| x.1).sum())
        } else {
            None
        }
    }
}

fn align_to(addr: usize, align: usize) -> (usize, usize) {
    debug_assert!(align != 0);
    debug_assert!(align.is_power_of_two());
    let mask = align - 1;

    if addr & mask == 0 {
        (addr, 0)
    } else {
        let fixed = addr.checked_add(mask).unwrap() & !mask;
        (fixed, fixed - addr)
    }
}
