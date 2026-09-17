// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use super::GPE0_BLK_ADDR;
use crate::common::{RWOp, ReadOp, WriteOp};
use std::sync::Mutex;

bitflags! {
    #[derive(Default, Copy, Clone)]
    struct GpeSts: u8{}
}

bitflags! {
    #[derive(Default, Copy, Clone)]
    struct GpeEn: u8{}
}

struct GpeRegisters {
    gpe0_en: GpeEn,
    gpe0_sts: GpeSts,
}

impl GpeRegisters {
    pub fn new() -> Self {
        Self { gpe0_sts: GpeSts::empty(), gpe0_en: GpeEn::empty() }
    }
}

pub struct Gpe {
    regs: Mutex<GpeRegisters>,
}

impl Gpe {
    pub fn new() -> Self {
        Self { regs: GpeRegisters::new().into() }
    }

    pub fn enable(&mut self, bit: u8) {
        let mut regs = self.regs.lock().unwrap();
        regs.gpe0_en.insert(GpeEn::from_bits_retain(1 << bit));
    }

    pub fn disable(&mut self, bit: u8) {
        let mut regs = self.regs.lock().unwrap();
        regs.gpe0_en.remove(GpeEn::from_bits_retain(1 << bit));
    }

    pub fn set(&mut self, bit: u8) {
        let mut regs = self.regs.lock().unwrap();
        regs.gpe0_sts.insert(GpeSts::from_bits_retain(1 << bit));
    }

    pub fn unset(&mut self, bit: u8) {
        let mut regs = self.regs.lock().unwrap();
        regs.gpe0_sts.remove(GpeSts::from_bits_retain(1 << bit));
    }

    pub fn pio_rw(&self, port: u16, rwo: RWOp) {
        match port {
            GPE0_BLK_ADDR => self.gpe_rw(rwo),
            _ => {
                panic!();
            }
        }
    }

    pub fn gpe_rw(&self, rwo: RWOp) {
        match rwo {
            RWOp::Read(ro) => self.gpe_read(ro),
            RWOp::Write(wo) => self.gpe_write(wo),
        }
    }

    pub fn gpe_read(&self, ro: &mut ReadOp) {
        let regs = self.regs.lock().unwrap();
        let bits = match ro.offset() {
            0 => regs.gpe0_sts.bits(),
            2 => regs.gpe0_en.bits(),
            _ => {
                panic!();
            }
        };
        ro.write_u8(bits);
    }

    pub fn gpe_write(&self, _wo: &WriteOp) {}
}
