// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::sync::{Arc, Mutex};

use crate::common::{RWOp, ReadOp, WriteOp};
use crate::intr_pins::IntrPin;

bitflags! {
    #[derive(Default, Copy, Clone, Debug)]
    struct StsRegister: u8{}
}

bitflags! {
    #[derive(Default, Copy, Clone, Debug)]
    struct EnRegister: u8{}
}

enum RegisterBlock {
    En(usize),
    Sts(usize),
}

impl RegisterBlock {
    fn name(&self) -> &'static str {
        match self {
            RegisterBlock::En(_) => "EN",
            RegisterBlock::Sts(_) => "STS",
        }
    }
}

#[derive(Debug)]
struct Registers {
    reg_len: usize,
    en: Vec<EnRegister>,
    sts: Vec<StsRegister>,
}

impl Registers {
    fn new(len: usize) -> Self {
        let reg_len = len / 2;
        Self {
            reg_len,
            sts: vec![StsRegister::empty(); reg_len],
            en: vec![EnRegister::empty(); reg_len],
        }
    }

    fn reset(&mut self) {
        self.sts.fill(StsRegister::empty());
        self.en.fill(EnRegister::empty());
    }

    fn raise(&mut self, bit: u16) {
        let offset = (bit / 8) as usize;
        assert!(offset < self.reg_len);

        let bits = StsRegister::from_bits_retain(1 << (bit % 8));
        self.sts[offset].insert(bits);
    }
}

pub struct Gpe {
    addr: u16,
    len: usize,
    regs: Mutex<Registers>,
    sci_pin: Arc<dyn IntrPin>,
}

impl Gpe {
    pub fn new(addr: u16, len: usize, sci_pin: Arc<dyn IntrPin>) -> Self {
        assert!(len > 0);
        assert!(len.is_multiple_of(2));
        Self { addr, len, regs: Registers::new(len).into(), sci_pin }
    }

    pub fn raise(&self, bit: u16) {
        let mut regs = self.regs.lock().unwrap();
        regs.raise(bit);
        self.update_sci(&regs);
    }

    pub fn reset(&self) {
        let mut regs = self.regs.lock().unwrap();
        regs.reset();
        self.update_sci(&regs);
    }

    pub fn pio_rw(&self, port: u16, rwo: RWOp) {
        assert_eq!(port, self.addr);
        self.gpe_rw(rwo);
    }

    fn gpe_rw(&self, rwo: RWOp) {
        match rwo {
            RWOp::Read(ro) => self.gpe_read(ro),
            RWOp::Write(wo) => self.gpe_write(wo),
        }
    }

    fn gpe_read(&self, ro: &mut ReadOp) {
        let regs = self.regs.lock().unwrap();
        let reg = self.register_block(ro.offset());
        let bits = match reg {
            RegisterBlock::Sts(x) => regs.sts[x].bits(),
            RegisterBlock::En(x) => regs.en[x].bits(),
        };
        ro.write_u8(bits);

        probes::gpe_read!(|| (self.addr, reg.name(), ro.offset(), bits));
    }

    fn gpe_write(&self, wo: &mut WriteOp) {
        let mut regs = self.regs.lock().unwrap();
        let bits = wo.read_u8();
        let reg = self.register_block(wo.offset());
        match reg {
            RegisterBlock::Sts(x) => {
                regs.sts[x].remove(StsRegister::from_bits_retain(bits))
            }
            RegisterBlock::En(x) => {
                regs.en[x] = EnRegister::from_bits_retain(bits)
            }
        };
        self.update_sci(&regs);

        probes::gpe_write!(|| (self.addr, reg.name(), wo.offset(), bits));
    }

    fn register_block(&self, offset: usize) -> RegisterBlock {
        match offset {
            x if x < self.len / 2 => RegisterBlock::Sts(x),
            x if x >= self.len / 2 && x < self.len => {
                RegisterBlock::En(x - self.len / 2)
            }
            _ => {
                unreachable!(
                    "unexpected GPE access on address {}, offset {}",
                    self.addr, offset
                );
            }
        }
    }

    fn update_sci(&self, regs: &Registers) {
        let active = regs
            .sts
            .iter()
            .zip(&regs.en)
            .any(|(sts, en)| sts.bits() & en.bits() != 0);
        self.sci_pin.set_state(active);
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    struct TestPin {
        asserted: AtomicBool,
    }

    impl TestPin {
        fn new() -> Self {
            Self { asserted: AtomicBool::new(false) }
        }
    }

    impl IntrPin for TestPin {
        fn assert(&self) {
            self.asserted.store(true, Ordering::SeqCst);
        }
        fn deassert(&self) {
            self.asserted.store(false, Ordering::SeqCst);
        }
        fn is_asserted(&self) -> bool {
            self.asserted.load(Ordering::SeqCst)
        }
        fn import_state(&self, _: bool) {
            todo!("implement when needed for testing");
        }
    }

    #[test]
    fn pio() {
        let pin = Arc::new(TestPin::new());
        let gpe = Gpe::new(0x1234, 4, pin.clone());
        let expected = [
            0b0010_1010, // STS[0]
            0b0100_0111, // STS[1]
            0b0000_1100, // EN[0]
            0b0100_0010, // EN[1]
        ];

        // Write EN bits.
        for offset in 2..=3 {
            let mut buf = [expected[offset]];
            let mut wo = WriteOp::from_buf(offset, &mut buf);
            gpe.pio_rw(0x1234, RWOp::Write(&mut wo));
        }
        // Raise STS bits.
        gpe.raise(1);
        gpe.raise(3);
        gpe.raise(5);
        gpe.raise(8);
        gpe.raise(9);
        gpe.raise(10);
        gpe.raise(14);
        {
            let regs = gpe.regs.lock().unwrap();
            assert_eq!(regs.sts[0].bits(), expected[0]);
            assert_eq!(regs.sts[1].bits(), expected[1]);
            assert_eq!(regs.en[0].bits(), expected[2]);
            assert_eq!(regs.en[1].bits(), expected[3]);
        }

        // Read bits. STS bits latch.
        for offset in 0..=3 {
            let mut buf = [0];
            let mut ro = ReadOp::from_buf(offset, &mut buf);
            gpe.pio_rw(0x1234, RWOp::Read(&mut ro));

            assert_eq!(
                buf[0], expected[offset],
                "unexpected value at offset {}",
                offset
            );
        }
        {
            let regs = gpe.regs.lock().unwrap();
            assert_eq!(regs.sts[0].bits(), expected[0]);
            assert_eq!(regs.sts[1].bits(), expected[1]);
            assert_eq!(regs.en[0].bits(), expected[2]);
            assert_eq!(regs.en[1].bits(), expected[3]);
        }

        // Write-1-to-clear STS.
        let mut buf = [0b0010_1000];
        let mut wo = WriteOp::from_buf(0, &mut buf);
        gpe.pio_rw(0x1234, RWOp::Write(&mut wo));

        buf = [0b0000_0110];
        wo = WriteOp::from_buf(1, &mut buf);
        gpe.pio_rw(0x1234, RWOp::Write(&mut wo));
        {
            let regs = gpe.regs.lock().unwrap();
            assert_eq!(regs.sts[0].bits(), 0b0000_0010);
            assert_eq!(regs.sts[1].bits(), 0b0100_0001);
        }

        // Guest can't raise STS.
        buf = [0b0010_1000];
        wo = WriteOp::from_buf(0, &mut buf);
        gpe.pio_rw(0x1234, RWOp::Write(&mut wo));

        buf = [0b0000_0110];
        wo = WriteOp::from_buf(1, &mut buf);
        gpe.pio_rw(0x1234, RWOp::Write(&mut wo));
        {
            let regs = gpe.regs.lock().unwrap();
            assert_eq!(regs.sts[0].bits(), 0b0000_0010);
            assert_eq!(regs.sts[1].bits(), 0b0100_0001);
        }

        // Clear registers.
        gpe.reset();
        {
            let regs = gpe.regs.lock().unwrap();
            assert_eq!(regs.sts[0].bits(), 0b0000_0000);
            assert_eq!(regs.sts[1].bits(), 0b0000_0000);
            assert_eq!(regs.en[0].bits(), 0b0000_0000);
            assert_eq!(regs.en[1].bits(), 0b0000_0000);
        }
    }

    #[test]
    fn sci() {
        let pin = Arc::new(TestPin::new());
        let gpe = Gpe::new(0x1234, 2, pin.clone());

        // SCI high if some bit has (STS & EN) true.
        let mut buf = [0b1111_1111]; // Enable all bits.
        let mut wo = WriteOp::from_buf(1, &mut buf);
        gpe.pio_rw(0x1234, RWOp::Write(&mut wo));
        // Raise bits 6 and 2.
        gpe.raise(2);
        gpe.raise(6);

        assert!(pin.is_asserted());

        // SCI low if no bit has (STS & EN) true.
        buf = [0b1011_1111]; // Disable bit 6.
        wo = WriteOp::from_buf(1, &mut buf);
        gpe.pio_rw(0x1234, RWOp::Write(&mut wo));

        buf = [0b0000_0100]; // Clear bit 2.
        wo = WriteOp::from_buf(0, &mut buf);
        gpe.pio_rw(0x1234, RWOp::Write(&mut wo));

        assert!(!pin.is_asserted());

        // SCI not raised if bit is not enabled.
        buf = [0b0000_0000]; // Disable all bits.
        wo = WriteOp::from_buf(1, &mut buf);
        gpe.pio_rw(0x1234, RWOp::Write(&mut wo));
        // Raise bit 2.
        gpe.raise(2);

        assert!(!pin.is_asserted());

        // SCI raised when bit enabled and STS latched.
        buf = [0b0000_0100]; // Enable bit 2 again.
        wo = WriteOp::from_buf(1, &mut buf);
        gpe.pio_rw(0x1234, RWOp::Write(&mut wo));

        assert!(pin.is_asserted());

        // SCI low after reset.
        gpe.reset();
        assert!(!pin.is_asserted());
    }

    #[test]
    #[should_panic]
    fn panic_length_zero() {
        let pin = Arc::new(TestPin::new());
        let _ = Gpe::new(0x1234, 0, pin.clone());
    }

    #[test]
    #[should_panic]
    fn panic_length_not_even() {
        let pin = Arc::new(TestPin::new());
        let _ = Gpe::new(0x1234, 3, pin.clone());
    }

    #[test]
    #[should_panic]
    fn panic_raise_invalid_bit() {
        let pin = Arc::new(TestPin::new());
        let gpe = Gpe::new(0x1234, 2, pin);
        gpe.raise(20);
    }

    #[test]
    #[should_panic]
    fn panic_pio_wrong_addr() {
        let pin = Arc::new(TestPin::new());
        let gpe = Gpe::new(0x1234, 2, pin);

        let mut buf = [0];
        let mut ro = ReadOp::from_buf(0, &mut buf);
        gpe.pio_rw(0x5678, RWOp::Read(&mut ro));
    }

    #[test]
    #[should_panic]
    fn panic_pio_wrong_offset() {
        let pin = Arc::new(TestPin::new());
        let gpe = Gpe::new(0x1234, 2, pin);

        let mut buf = [0];
        let mut ro = ReadOp::from_buf(99, &mut buf);
        gpe.pio_rw(0x1234, RWOp::Read(&mut ro));
    }
}

#[usdt::provider(provider = "propolis")]
mod probes {
    fn gpe_read(addr: u16, reg: &str, offset: usize, value: u8) {}
    fn gpe_write(addr: u16, reg: &str, offset: usize, value: u8) {}
}
