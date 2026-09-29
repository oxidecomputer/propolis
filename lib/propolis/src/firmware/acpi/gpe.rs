// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::sync::{Arc, Mutex};

use crate::common::RWOp;
use crate::intr_pins::IntrPin;
use crate::migrate::*;
use crate::util::regmap::RegMap;

/// Represents an ACPI GPEx_BLK register block.
///
/// Register ownership is shared between guests and the platform.
///
/// Guests read/write from/to the GPEx_EN registers and read/write-1-to-clear
/// from/to the GPEx_STS registers.
///
/// The platform reads from GPEx_EN and writes to GPEx_STS when a
/// general-purpose event should be triggered.
///
/// The SCI pin is raised if any bit position has both its GPEx_EN bit and
/// GPEx_STS set.
///
/// An overview of operations looks as follows.
/// - Guest resets GPEx_STS registers on boot by writing 0xFF.
/// - Guest resets GPEx_EN registers on boot by writing 0x00.
/// - Guest enables bits in GPEx_EN registers that have corresponding
///   _Exx/_Lxx ACPI methods defined.
/// - Platform raises corresponding GPEx_STS bit when a general-purpose event
///   happens.
/// - Platform asserts the SCI pin if GPEx_STS & GPEx_EN != 0.
/// - Guest reads GPEx_STS and GPEx_EN to determine which ACPI methods must be
///   executed.
///
/// Refer to the following sections of the ACPI spec. v6.6 for more
/// information.
/// - Sec. 4.8 - ACPI Register Model
/// - Sec. 4.8.1.6 - General-Purpose Event Registers
/// - Sec. 4.8.4.1 - General-Purpose Event Register Blocks
/// - Sec. 5.6.4 - General-Purpose Event Handling
pub struct RegisterBlock {
    map: RegMap<Register>,
    inner: Mutex<RegisterBlockInner>,
}
impl RegisterBlock {
    /// Creates a new GPEx_BLK. `addr` and `len` must match the values for
    /// `GPEx_BLK` (or `X_GPEx_BLK`) and `GPEx_BLK_LEN` in the FADT.
    ///
    /// # Panics
    ///
    /// Panics if `len` is zero, not divisible by 2, or greater than 32, or
    /// if `addr` is not 32-bit aligned.
    pub fn new(addr: u16, len: usize, sci_pin: Arc<dyn IntrPin>) -> Self {
        // From the ACPI spec. sec. 5.6.4:
        //
        // An OEM can implement up to 128 general-purpose event inputs in
        // hardware per GPE block, each as either a level or edge event. It is
        // also possible to implement a single 256-pin block as long as it's
        // the only block defined in the system.
        //
        // 128 GPEs per block requires 16 bytes per register, and each block
        // has two registers (GPEx_STS and GPEx_EN). It is also possible to
        // have two GPE blocks per machine (GPE0 and GPE1), so assume a maximum
        // of 128 GPEs per block.
        assert!(len > 0);
        assert!(len.is_multiple_of(2));
        assert!(len <= 32);

        // From the ACPI spec. sec. 4.8.4.1:
        //
        // Each register block is 32-bit aligned.
        assert!(addr.is_multiple_of(4));

        let reg_len = len / 2;
        let mut map = RegMap::new(len);
        for i in 0..reg_len {
            map.define(i, 1, Register::Sts(i));
            map.define(reg_len + i, 1, Register::En(i));
        }

        Self {
            map,
            inner: RegisterBlockInner::new(addr, reg_len, sci_pin).into(),
        }
    }

    /// Raises bit position for a GPE in the GPEx_STS register and updates the
    /// SCI state if necessary.
    ///
    /// # Panics
    ///
    /// Panics if `gpe` is greater than or equal to GPEx_STS size
    /// (8 * GPEx_LEN / 2).
    pub fn raise(&self, gpe: u8) {
        self.inner.lock().unwrap().raise(gpe);
    }

    /// Clears all bits in GPEx_STS and GPEx_EN and updates the SCI state if
    /// necessary.
    pub fn reset(&self) {
        self.inner.lock().unwrap().reset();
    }

    /// Handle port I/O operation.
    pub(super) fn pio_rw(&self, _: u16, mut rwo: RWOp) {
        let mut inner = self.inner.lock().unwrap();

        self.map.process(&mut rwo, |reg, rwo| match rwo {
            RWOp::Read(ro) => ro.write_u8(inner.read(*reg)),
            RWOp::Write(wo) => inner.write(*reg, wo.read_u8()),
        });

        // Only update the SCI state after processing the entire operation
        // instead of on each access to prevent spurious updates.
        inner.update_sci();
    }

    fn export(&self) -> migrate::RegisterBlockV1 {
        let inner = self.inner.lock().unwrap();
        migrate::RegisterBlockV1 {
            sts: inner.sts.clone(),
            en: inner.en.clone(),
        }
    }

    fn import(
        &self,
        state: migrate::RegisterBlockV1,
    ) -> Result<(), MigrateStateError> {
        let mut inner = self.inner.lock().unwrap();

        if state.sts.len() != inner.sts.len() {
            return Err(MigrateStateError::ImportFailed(format!(
                "ACPI GPE: expected STS register block of size {}, got {}",
                inner.sts.len(),
                state.sts.len()
            )));
        }
        if state.en.len() != inner.en.len() {
            return Err(MigrateStateError::ImportFailed(format!(
                "ACPI GPE: expected EN register block of size {}, got {}",
                inner.en.len(),
                state.en.len()
            )));
        }

        inner.sts = state.sts;
        inner.en = state.en;
        inner.import_sci_state();
        Ok(())
    }
}
impl MigrateMulti for RegisterBlock {
    fn export(
        &self,
        output: &mut PayloadOutputs,
        _: &MigrateCtx,
    ) -> Result<(), MigrateStateError> {
        output.push(self.export().into())
    }

    fn import(
        &self,
        offer: &mut PayloadOffers,
        _: &MigrateCtx,
    ) -> Result<(), MigrateStateError> {
        self.import(offer.take()?)
    }
}

struct RegisterBlockInner {
    addr: u16,

    sts: Vec<u8>,
    en: Vec<u8>,

    sci_pin: Arc<dyn IntrPin>,
}
impl RegisterBlockInner {
    fn new(addr: u16, reg_len: usize, sci_pin: Arc<dyn IntrPin>) -> Self {
        Self { addr, sts: vec![0; reg_len], en: vec![0; reg_len], sci_pin }
    }

    fn raise(&mut self, gpe: u8) {
        let offset = usize::from(gpe / 8);
        assert!(offset < self.sts.len(), "invalid GPE number {gpe}");

        let reg = Register::Sts(offset);
        let gpe_mask = 1 << (gpe % 8);
        self[reg] |= gpe_mask;

        probes::gpe_sts_raise!(|| (
            self.addr,
            gpe,
            self[Register::Sts(offset)],
            self[Register::En(offset)],
        ));

        // Update SCI after probe so the events are logged in order.
        self.update_sci();
    }

    fn reset(&mut self) {
        self.sts.fill(0);
        self.en.fill(0);

        probes::gpe_reset!(|| self.addr);

        // Update SCI after probe so the events are logged in order.
        self.update_sci();
    }

    fn read(&self, reg: Register) -> u8 {
        let bits = self[reg];
        probes::gpe_read!(|| (self.addr, reg.name(), reg.index(), bits));
        bits
    }

    fn write(&mut self, reg: Register, bits: u8) {
        let old = self[reg];
        match reg {
            // GPEx_STS is write-1-to-clear.
            Register::Sts(_) => self[reg] &= !bits,
            Register::En(_) => self[reg] = bits,
        };
        let new = self[reg];

        probes::gpe_write!(|| (
            self.addr,
            reg.name(),
            reg.index(),
            bits,
            old,
            new,
        ));
    }

    /// Asserts the SCI pin if GPEx_STS & GPEx_EN != 0.
    fn update_sci(&self) {
        let should_assert = self.should_assert_sci();
        if should_assert != self.sci_pin.is_asserted() {
            self.sci_pin.set_state(should_assert);
            probes::gpe_update_sci!(|| (self.addr, should_assert));
        }
    }

    /// Sets the SCI pin state without triggering an edge.
    fn import_sci_state(&self) {
        self.sci_pin.import_state(self.should_assert_sci());
    }

    /// Returns true if GPEx_STS & GPEx_EN != 0.
    fn should_assert_sci(&self) -> bool {
        self.sts.iter().zip(&self.en).any(|(sts, en)| sts & en != 0)
    }
}
impl std::ops::Index<Register> for RegisterBlockInner {
    type Output = u8;

    fn index(&self, reg: Register) -> &Self::Output {
        match reg {
            Register::Sts(i) => &self.sts[i],
            Register::En(i) => &self.en[i],
        }
    }
}
impl std::ops::IndexMut<Register> for RegisterBlockInner {
    fn index_mut(&mut self, reg: Register) -> &mut Self::Output {
        match reg {
            Register::Sts(i) => &mut self.sts[i],
            Register::En(i) => &mut self.en[i],
        }
    }
}

/// Represents a byte in a GPEx register.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Register {
    /// Byte i of GPEx_STS.
    Sts(usize),
    /// Byte i of GPEx_EN.
    En(usize),
}
impl Register {
    const fn name(&self) -> &'static str {
        match self {
            Register::Sts(_) => "STS",
            Register::En(_) => "EN",
        }
    }
    const fn index(&self) -> usize {
        match self {
            Register::Sts(i) | Register::En(i) => *i,
        }
    }
}

pub mod migrate {
    use crate::migrate::*;
    use serde::{Deserialize, Serialize};

    #[derive(Debug, Serialize, Deserialize)]
    pub struct RegisterBlockV1 {
        pub sts: Vec<u8>,
        pub en: Vec<u8>,
    }
    impl Schema<'_> for RegisterBlockV1 {
        fn id() -> SchemaId {
            ("acpi-gpe", 1)
        }
    }
}

#[cfg(test)]
pub mod test {
    use super::*;
    use crate::common::{ReadOp, WriteOp};
    use crate::intr_pins::test::TestPin;

    pub(crate) fn pio_write(block: &RegisterBlock, reg: Register, data: u8) {
        let buf = [data];
        let offset = reg_offset(&block.inner.lock().unwrap(), reg);
        let mut wo = WriteOp::from_buf(offset, &buf);
        block.pio_rw(0, RWOp::Write(&mut wo));
    }

    pub(crate) fn pio_read(block: &RegisterBlock, reg: Register) -> u8 {
        let mut buf = [0];
        let offset = reg_offset(&block.inner.lock().unwrap(), reg);
        let mut ro = ReadOp::from_buf(offset, &mut buf);
        block.pio_rw(0, RWOp::Read(&mut ro));
        buf[0]
    }

    pub(crate) fn raise_sts(block: &RegisterBlock, idx: usize, data: u8) {
        (0..8)
            // Find bits set to 1.
            .filter(|bit| (data & (1 << bit)) != 0)
            // Raise bits set to 1.
            .for_each(|bit| block.raise((bit + idx * 8) as u8));
    }

    fn reg_offset(inner: &RegisterBlockInner, reg: Register) -> usize {
        match reg {
            Register::Sts(i) => i,
            Register::En(i) => inner.sts.len() + i,
        }
    }

    #[test]
    fn pio() {
        let pin = Arc::new(TestPin::new());
        let gpe = RegisterBlock::new(0x1234, 4, pin.clone());

        let expected_sts = [
            0b0010_1010, // STS[0]
            0b0100_0111, // STS[1]
        ];
        let expected_en = [
            0b0000_1100, // EN[0]
            0b0100_0010, // EN[1]
        ];
        let assert_inner_state = || {
            let inner = gpe.inner.lock().unwrap();
            assert_eq!(inner[Register::Sts(0)], expected_sts[0]);
            assert_eq!(inner[Register::Sts(1)], expected_sts[1]);
            assert_eq!(inner[Register::En(0)], expected_en[0]);
            assert_eq!(inner[Register::En(1)], expected_en[1]);
        };

        // Guest writes EN bits.
        pio_write(&gpe, Register::En(0), expected_en[0]);
        pio_write(&gpe, Register::En(1), expected_en[1]);

        // Platform raises STS bits.
        raise_sts(&gpe, 0, expected_sts[0]);
        raise_sts(&gpe, 1, expected_sts[1]);
        assert_inner_state();

        // Guest reads STS bits.
        // STS bits latch, so they stay the same.
        let sts0 = pio_read(&gpe, Register::Sts(0));
        let sts1 = pio_read(&gpe, Register::Sts(1));

        assert_eq!(sts0, expected_sts[0]);
        assert_eq!(sts1, expected_sts[1]);
        assert_inner_state();

        // Guest writes 1 to clear STS bits.
        pio_write(&gpe, Register::Sts(0), 0b0010_1000); // Clears bits 3 and 5.
        pio_write(&gpe, Register::Sts(1), 0b0000_0110); // Clears bits 1 and 2.
        {
            let inner = gpe.inner.lock().unwrap();
            assert_eq!(inner[Register::Sts(0)], 0b0000_0010);
            assert_eq!(inner[Register::Sts(1)], 0b0100_0001);
        }

        // Guest can't raise STS.
        pio_write(&gpe, Register::Sts(0), 0b0010_1000);
        pio_write(&gpe, Register::Sts(1), 0b0010_1000);
        {
            // Asserts STS bits stay the same.
            let inner = gpe.inner.lock().unwrap();
            assert_eq!(inner[Register::Sts(0)], 0b0000_0010);
            assert_eq!(inner[Register::Sts(1)], 0b0100_0001);
        }

        // Clear registers.
        gpe.reset();
        {
            let inner = gpe.inner.lock().unwrap();
            assert_eq!(inner[Register::Sts(0)], 0b0000_0000);
            assert_eq!(inner[Register::Sts(1)], 0b0000_0000);
            assert_eq!(inner[Register::En(0)], 0b0000_0000);
            assert_eq!(inner[Register::En(1)], 0b0000_0000);
        }
    }

    #[test]
    fn pio_wide() {
        let pin = Arc::new(TestPin::new());
        let gpe = RegisterBlock::new(0x1234, 4, pin.clone());
        pio_write(&gpe, Register::En(0), 0xff);
        pio_write(&gpe, Register::En(1), 0xff);
        raise_sts(&gpe, 0, 0b0000_0011);
        raise_sts(&gpe, 1, 0b1000_0000);

        // Read STS[0], STS[1], EN[0], and EN[1] in a single operation.
        let mut buf = [0; 4];
        gpe.pio_rw(0, RWOp::Read(&mut ReadOp::from_buf(0, &mut buf)));
        assert_eq!(buf, [0b0000_0011, 0b1000_0000, 0xff, 0xff]);

        // Write-1-to-clear both STS registers in a single operation.
        let buf = [0xff, 0xff];
        gpe.pio_rw(0, RWOp::Write(&mut WriteOp::from_buf(0, &buf)));
        assert_eq!(pio_read(&gpe, Register::Sts(0)), 0);
        assert_eq!(pio_read(&gpe, Register::Sts(1)), 0);
        assert_eq!(pio_read(&gpe, Register::En(0)), 0xff);
        assert_eq!(pio_read(&gpe, Register::En(1)), 0xff);
        assert!(!pin.is_asserted());
    }

    #[test]
    fn sci() {
        let pin = Arc::new(TestPin::new());
        let gpe = RegisterBlock::new(0x1234, 2, pin.clone());

        // Guest enables all bits.
        pio_write(&gpe, Register::En(0), 0b1111_1111);
        // Platform raises bits 2 and 6.
        gpe.raise(2);
        gpe.raise(6);

        // SCI high since bits 2 and 6 have (STS & EN) true.
        assert!(pin.is_asserted());

        // Guest writes 1 to clear bit 2.
        pio_write(&gpe, Register::Sts(0), 0b0000_0100);
        // Guest disables bit 6.
        pio_write(&gpe, Register::En(0), 0b1011_1111);

        // SCI low since bit 2 is clear and bit 6 is disabled.
        // All bits have (STS & EN) false.
        assert!(!pin.is_asserted());

        // SCI not raised if bit is not enabled.
        pio_write(&gpe, Register::En(0), 0b0000_0000); // Disable all bits.
        gpe.raise(2); // Raise bit 2.
        assert!(!pin.is_asserted());

        // SCI raised when bit has (EN & STS) true.
        pio_write(&gpe, Register::En(0), 0b0000_0100); // Enable bit 2 again.
        assert!(pin.is_asserted());

        // SCI low after reset.
        gpe.reset();
        assert!(!pin.is_asserted());
    }

    #[test]
    #[should_panic = "len > 0"]
    fn panic_len_zero() {
        let pin = Arc::new(TestPin::new());
        let _ = RegisterBlock::new(0x1234, 0, pin.clone());
    }

    #[test]
    #[should_panic = "len.is_multiple_of(2)"]
    fn panic_len_not_even() {
        let pin = Arc::new(TestPin::new());
        let _ = RegisterBlock::new(0x1234, 3, pin.clone());
    }

    #[test]
    #[should_panic = "len <= 32"]
    fn panic_len_too_big() {
        let pin = Arc::new(TestPin::new());
        let _ = RegisterBlock::new(0x1234, 40, pin.clone());
    }

    #[test]
    #[should_panic = "addr.is_multiple_of(4)"]
    fn panic_addr_invalid() {
        let pin = Arc::new(TestPin::new());
        let _ = RegisterBlock::new(0x123, 4, pin.clone());
    }

    #[test]
    #[should_panic = "invalid GPE number"]
    fn panic_raise_invalid_bit() {
        let pin = Arc::new(TestPin::new());
        let gpe = RegisterBlock::new(0x1234, 2, pin);
        gpe.raise(20);
    }

    #[test]
    #[should_panic]
    fn panic_pio_wrong_offset() {
        let pin = Arc::new(TestPin::new());
        let gpe = RegisterBlock::new(0x1234, 2, pin);

        let mut buf = [0];
        let mut ro = ReadOp::from_buf(99, &mut buf);
        gpe.pio_rw(0x1234, RWOp::Read(&mut ro));
    }

    #[test]
    #[should_panic = "expected STS register block of size"]
    fn import_invalid_sts() {
        let payload =
            migrate::RegisterBlockV1 { sts: vec![0x00], en: vec![0x00, 0x00] };

        // Register block with len = 4 expects payload to have vecs of len 2.
        let pin = Arc::new(TestPin::new());
        RegisterBlock::new(0x1234, 4, pin.clone()).import(payload).unwrap();
    }

    #[test]
    #[should_panic = "expected EN register block of size"]
    fn import_invalid_en() {
        let payload =
            migrate::RegisterBlockV1 { sts: vec![0x00], en: vec![0x00, 0x00] };

        // Register block with len=2 expects payload to have vecs of len 1.
        let pin = Arc::new(TestPin::new());
        RegisterBlock::new(0x1234, 2, pin.clone()).import(payload).unwrap();
    }
}

#[usdt::provider(provider = "propolis")]
mod probes {
    fn gpe_read(addr: u16, reg: &str, idx: usize, value: u8) {}
    fn gpe_write(
        addr: u16,
        reg: &str,
        idx: usize,
        value: u8,
        old: u8,
        new: u8,
    ) {
    }
    fn gpe_sts_raise(addr: u16, bit: u8, sts: u8, en: u8) {}
    fn gpe_update_sci(addr: u16, state: bool) {}
    fn gpe_reset(addr: u16) {}
}
