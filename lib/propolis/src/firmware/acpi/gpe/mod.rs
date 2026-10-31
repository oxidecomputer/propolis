// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Module used to represent and manage ACPI general-purpose events (GPEs) and
//! related registers.
//!
//! GPEs are organized into register blocks, and a system can have up to two
//! register blocks: `GPE0` and `GPE1`. The information for each register block
//! is stored in the ACPI FADT table. The register block address is stored in
//! the value `GPEx_BLK` (or `X_GPEx_BLK` for longer addresses), and the
//! register block length is stored in `GPEx_BLK_LEN`.
//!
//! The register blocks are further split into two registers: `GPEx_STS`
//! (status) and `GPEx_EN` (enable) with each register having
//! `GPEx_BLK_LEN / 2` bytes in length.
//!
//! It's possible to define up to 128 GPEs per block, or 256 GPEs if the system
//! uses a single block. GPEs are split between `GPE0` and `GPE1` based on the
//! value `GPE1_BASE` in the FADT.
//!
//! ```text
//! Registers    Register Blocks
//! GPE0_STS─┬───GPE0
//!  GPE0_EN─┘
//! GPE1_STS─┬───GPE1
//!  GPE1_EN─┘
//! ```
//!
//! Register ownership is shared between the guest and the platform. Guests
//! read/write from/to `GPEx_EN` and read/write-1-to-clear (W1C) from/to
//! `GPEx_STS`. The platform reads from `GPEx_EN` and writes 1 to `GPEx_STS`.
//!
//! GPEs use the SCI pin to raise interrupts. The platform asserts the SCI when
//! any bit position is set to 1 in both `GPEx_STS` and `GPEx_EN`. Each GPE is
//! usually handled by a corresponding ACPI method defined in the DSDT or an
//! SSDT table. The method is mapped to the GPE registers by its name. For
//! example, the method `_E01` is mapped to bit position 1 of `GPEx_STS` and
//! `GPEx_EN` (bit positions are zero-indexed). The `E` prefix defines the GPE
//! end-of-interrupt (EOI) behaviour. `_Exx` methods are edge-triggered, while
//! `_Lxx` methods are level-triggered.
//!
//! An overview of operations looks as follows.
//!
//! - Guest resets `GPEx_STS` on boot by writing `0xFF` to each byte.
//! - Guest resets `GPEx_EN` on boot by writing `0x00` to each byte.
//! - Guest writes 1 to bits in `GPEx_EN` that have a corresponding `_Exx` or
//!   `_Lxx` ACPI method.
//! - Platform writes 1 to the corresponding bit position of `GPEx_STS` when a
//!   GPE needs to be triggered.
//! - Platform reads `GPEx_EN` and `GPEx_STS` and asserts the SCI pin if
//!   `GPEx_STS & GPEx_EN != 0`.
//! - Guest receives the interrupt and reads `GPEx_STS` and `GPEx_EN` to
//!   determine which ACPI methods must be executed.
//! - Guest disables the event source by writing 0 to the GPE bit position in
//!   `GPEx_EN`.
//! - If edge event (`_Exx`), guest writes 1 to the GPE bit position in
//!   `GPEx_STS` to clear the GPE status.
//! - Guest runs the GPE event handler ACPI method.
//! - If level event (`_Lxx`), guest waits for event handler to finish and
//!   writes 1 to the GPE bit position in `GPEx_STS` to clear the GPE status.
//! - If the line of a level-triggered GPE is still asserted, the platform
//!   keeps its bit position in `GPEx_STS` set.
//! - Guest re-enables event source by writing 1 to the GPE bit in `GPEx_EN`.
//!
//! A GPE register block is represented by [`RegisterBlock`] and GPEs by
//! [`Event`]. The method [`RegisterBlock::trigger`] can be used to activate
//! edge-triggered events and [`RegisterBlock::set_level`] can be used to
//! assert and deassert the GPE line for level-triggered events.
//!
//! Refer to the following sections of the ACPI spec. v6.6 for more
//! information.
//!
//! - Sec. 4.8 - ACPI Register Model
//! - Sec. 4.8.1.6 - General-Purpose Event Registers
//! - Sec. 4.8.4.1 - General-Purpose Event Register Blocks
//! - Sec. 5.6.4 - General-Purpose Event Handling
//!
//! # Migration
//!
//! The internal state of the registers in [`RegisterBlock`] is migrated to the
//! destination and the SCI state is recomputed but not re-asserted, meaning
//! that the destination VM will not receive another interrupt.
//!
//! The line state of level-triggered events is NOT migrated. The devices that
//! control them MUST call [`RegisterBlock::import_level`] to re-assert their
//! state. The call can happen before or after the [`RegisterBlock`] state is
//! imported.

use std::ops::RangeInclusive;
use std::sync::{Arc, Mutex};

use crate::common::RWOp;
use crate::intr_pins::IntrPin;
use crate::migrate::*;
use crate::util::regmap::RegMap;

#[cfg(test)]
mod acpica_sim;

/// Represents a GPE interrupt trigger type.
#[derive(Copy, Clone, Debug, PartialEq)]
pub enum Trigger {
    Edge,
    Level,
}
impl Trigger {
    pub const fn to_char(self) -> char {
        match self {
            Trigger::Edge => 'E',
            Trigger::Level => 'L',
        }
    }
}

/// Stores information about a GPE.
#[derive(Copy, Clone, Debug)]
pub struct Event {
    number: u8,
    trigger: Trigger,
}
impl Event {
    pub const fn new(number: u8, trigger: Trigger) -> Self {
        Self { number, trigger }
    }

    pub const fn number(self) -> u8 {
        self.number
    }

    pub const fn trigger(self) -> Trigger {
        self.trigger
    }
}

/// Represents an ACPI GPEx register block.
pub struct RegisterBlock {
    /// The range of GPE numbers handled by this register block.
    /// (base..=base + 8 * len / 2 - 1)
    gpe_range: RangeInclusive<u8>,
    map: RegMap<Register>,
    inner: Mutex<RegisterBlockInner>,
}
impl RegisterBlock {
    /// Creates a new GPE register block. Attributes must match values in the
    /// FADT table.
    ///
    /// `addr` is the system port address for the register block and must match
    /// `GPEx_BLK` (or `X_GPEx_BLK`) in the FADT.
    ///
    /// `len` is the total number of bytes to use for the registers. Must match
    /// `GPEx_BLK_LEN`.
    ///
    /// `base` is the base value of GPE event numbers that are handled by the
    /// register block. Must be zero for GPE0 register blocks or match
    /// `GPE1_BASE` for GPE1.
    ///
    /// # Panics
    ///
    /// Panics if `len` is zero, not divisible by 2, or greater than 64, or if
    /// the total number of GPEs handled by the register block
    /// (`base + 8 * len / 2`) is greater than 256, or if `addr` is not 32-bit
    /// aligned.
    pub fn new(
        addr: u16,
        len: usize,
        base: u8,
        sci_pin: Arc<dyn IntrPin>,
    ) -> Self {
        // From the ACPI spec. sec. 5.6.4:
        //
        // "An OEM can implement up to 128 general-purpose event inputs in
        // hardware per GPE block, each as either a level or edge event. It is
        // also possible to implement a single 256-pin block as long as it's
        // the only block defined in the system."
        //
        // And from sec. 5.6.4.1:
        //
        // "The event values for status bits in GPE0_BLK start at zero (_T00)
        // and end at the (GPE0_BLK_LEN / 2) - 1. The event values for status
        // bits in GPE1_BLK start at GPE1_BASE and end at GPE1_BASE +
        // (GPE1_BLK_LEN / 2) - 1.".
        //
        // `len` is evenly split between `GPEx_STS` and `GPEx_EN`, so it must
        // be divisible by two. And since we don't know how callers are
        // splitting GPEs across GPE0 and GPE1, we check for the maximum number
        // of bytes allowed in the case where only GPE0 is used (64 bytes).
        assert!(len > 0);
        assert!(len.is_multiple_of(2));
        assert!(len <= 64);

        let reg_len = len / 2;
        let last_gpe = usize::from(base) + 8 * reg_len - 1;

        // From the ACPI spec. sec. 5.6.4.1.1:
        //
        // "The GPEx_STS bits in the GPEx_BLK are indexed with a number from 0
        // through FF."
        //
        // So only 256 GPEs are actually usable.
        assert!(last_gpe < 256);

        // From the ACPI spec. sec. 4.8.4.1:
        //
        // "Each register block is 32-bit aligned."
        assert!(addr.is_multiple_of(4));

        let mut map = RegMap::new(len);
        for i in 0..reg_len {
            map.define(i, 1, Register::Sts(i));
            map.define(reg_len + i, 1, Register::En(i));
        }

        Self {
            map,
            gpe_range: (base..=last_gpe as u8), // u8 conversion checked above.
            inner: RegisterBlockInner::new(addr, reg_len, sci_pin).into(),
        }
    }

    /// Triggers an edge-triggered GPE.
    ///
    /// # Panics
    ///
    /// Panics if the [`Event`] is not [`Trigger::Edge`] or if the GPE number
    /// is less than the register block base, or greater than the maximum GPE
    /// number handled by this register block (`base + 8 * len / 2 - 1`).
    pub fn trigger(&self, gpe: Event) {
        let bit = self.gpe_bit(gpe, Trigger::Edge);
        self.inner.lock().unwrap().raise(bit);
    }

    /// Sets the level of a level-triggered GPE. GPE status bits latch, so
    /// deasserting a GPE will not clear its `GPEx_STS` bit.
    ///
    /// # Panics
    ///
    /// Panics if the [`Event`] is not [`Trigger::Level`] or if the GPE number
    /// is less than the register block base, or greater than the maximum GPE
    /// number handled by this register block (`base + 8 * len / 2 - 1`).
    pub fn set_level(&self, gpe: Event, is_asserted: bool) {
        let bit = self.gpe_bit(gpe, Trigger::Level);
        self.inner.lock().unwrap().set_level(bit, is_asserted);
    }

    /// Imports the level of a level-triggered GPE. Similar to
    /// [`RegisterBlock::set_level`] but it doesn't affect the SCI.
    ///
    /// # Panics
    ///
    /// Refer to [`RegisterBlock::set_level`] for more information.
    pub fn import_level(&self, gpe: Event, is_asserted: bool) {
        let bit = self.gpe_bit(gpe, Trigger::Level);
        self.inner.lock().unwrap().import_level(bit, is_asserted);
    }

    /// Clears all bits in GPEx_EN and the bits in GPEx_STS with
    /// level-triggered line not asserted. Updates the SCI state if necessary.
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

        let reg_lens = [
            ("STS", inner.sts.len(), state.sts.len()),
            ("EN", inner.en.len(), state.en.len()),
        ];
        for (reg, expected, got) in reg_lens {
            if got != expected {
                return Err(MigrateStateError::ImportFailed(format!(
                    "ACPI GPE: expected {} register block of size {}, got {}",
                    reg, expected, got,
                )));
            }
        }

        inner.sts = state.sts;
        inner.en = state.en;

        inner.import_sci_state();

        Ok(())
    }

    fn gpe_bit(&self, gpe: Event, trigger: Trigger) -> u8 {
        assert!(
            self.gpe_range.contains(&gpe.number()),
            "invalid GPE number {} for register block with GPE range {:?}",
            gpe.number(),
            self.gpe_range,
        );
        assert_eq!(gpe.trigger(), trigger, "event {gpe:?} is not {trigger:?}");

        gpe.number() - self.gpe_range.start()
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

/// Internal representation of a GPE register block.
///
/// The data for each register is stored as a `Vec<u8>` because, as defined in
/// the ACPI spec., registers are accessed per byte, regardless of their
/// length. Each register's length is also dynamic, and must match the value of
/// `GPEx_BLK_LEN / 2` from the FADT table.
struct RegisterBlockInner {
    addr: u16,

    sts: Vec<u8>,
    en: Vec<u8>,

    // Stores the line level for level-triggered GPEs. While a level bit is
    // set, it's not possible to clear its corresponding GPEx_STS bit.
    level: Vec<u8>,

    sci_pin: Arc<dyn IntrPin>,
}
impl RegisterBlockInner {
    fn new(addr: u16, reg_len: usize, sci_pin: Arc<dyn IntrPin>) -> Self {
        Self {
            addr,
            sts: vec![0; reg_len],
            en: vec![0; reg_len],
            level: vec![0; reg_len],
            sci_pin,
        }
    }

    fn raise(&mut self, gpe: u8) {
        let reg = usize::from(gpe / 8);
        let mask = 1 << (gpe % 8);

        self.sts[reg] |= mask;

        probes::gpe_sts_raise!(|| (
            self.addr,
            gpe,
            self.sts[reg],
            self.en[reg],
        ));

        // Update SCI after probe so the events are logged in order.
        self.update_sci();
    }

    fn set_level(&mut self, gpe: u8, is_asserted: bool) {
        self.import_level(gpe, is_asserted);
        self.update_sci();
    }

    fn import_level(&mut self, gpe: u8, is_asserted: bool) {
        let reg = usize::from(gpe / 8);
        let mask = 1 << (gpe % 8);

        if is_asserted {
            self.level[reg] |= mask;
            self.sts[reg] |= mask;
        } else {
            self.level[reg] &= !mask;
            // Only the guest clears GPEx_STS.
        }

        probes::gpe_set_level!(|| (
            self.addr,
            gpe,
            self.sts[reg],
            self.en[reg],
            self.level[reg],
        ));
    }

    fn reset(&mut self) {
        // Reset registers, but keep bits in GPEx_STS set if their line is
        // still asserted.
        self.sts = self.level.clone();
        self.en.fill(0);

        probes::gpe_reset!(|| self.addr);

        // Update SCI after probe so the events are logged in order.
        self.update_sci();
    }

    fn read(&self, reg: Register) -> u8 {
        let bits = self.reg(reg);
        probes::gpe_read!(|| (self.addr, reg.name(), reg.index(), bits));
        bits
    }

    fn write(&mut self, reg: Register, bits: u8) {
        let old = self.reg(reg);
        match reg {
            Register::Sts(i) => {
                // GPEx_STS is write-1-to-clear.
                self.sts[i] &= !bits;

                // Keep GPEx_STS set if the level-triggered GPE line is still
                // asserted.
                self.sts[i] |= self.level[i];
            }
            Register::En(i) => self.en[i] = bits,
        };
        let new = self.reg(reg);

        probes::gpe_write!(|| (
            self.addr,
            reg.name(),
            reg.index(),
            bits,
            old,
            new,
        ));
    }

    /// Asserts or deasserts the SCI pin based on `GPEx_STS & GPEx_EN != 0`.
    fn update_sci(&self) {
        let should_assert = self.should_assert_sci();
        if should_assert != self.sci_pin.is_asserted() {
            self.sci_pin.set_state(should_assert);
            probes::gpe_update_sci!(|| (self.addr, u8::from(should_assert)));
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

    /// Returns the data stored in the [`Register`] `reg`.
    fn reg(&self, reg: Register) -> u8 {
        match reg {
            Register::Sts(i) => self.sts[i],
            Register::En(i) => self.en[i],
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
    const fn index(&self) -> u8 {
        match self {
            Register::Sts(i) | Register::En(i) => *i as u8,
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
    use crate::intr_pins::IntrPin;
    use acpica_sim::AcpicaSim;

    #[test]
    fn pio() {
        let (gpe0, _) = test_gpe0(4);
        let gpes = (0x00..0x0f)
            .map(|i| Event::new(i, Trigger::Edge))
            .collect::<Vec<Event>>();

        // Guest writes to EN to enable GPEs.
        // Enable GPEs 0x02 and 0x03.
        pio_write(&gpe0, Register::En(0), 0b0000_1100);
        // Enable GPEs 0x09 and 0x0E.
        pio_write(&gpe0, Register::En(1), 0b0100_0010);

        // Platform triggers events to raise STS bits.
        [
            // GPEs in STS[0].
            gpes[0x01], gpes[0x03], gpes[0x05],
            // GPEs in STS[1].
            gpes[0x08], gpes[0x09], gpes[0x0a], gpes[0x0e],
        ]
        .iter()
        .for_each(|gpe| {
            gpe0.trigger(*gpe);
        });
        assert_eq!(pio_read(&gpe0, Register::Sts(0)), 0b0010_1010);
        assert_eq!(pio_read(&gpe0, Register::Sts(1)), 0b0100_0111);
        assert_eq!(pio_read(&gpe0, Register::En(0)), 0b0000_1100);
        assert_eq!(pio_read(&gpe0, Register::En(1)), 0b0100_0010);

        // Guest W1C STS bits.
        pio_write(&gpe0, Register::Sts(0), 0b0010_1000); // Clears GPEs 0x03 and 0x05.
        pio_write(&gpe0, Register::Sts(1), 0b0000_0110); // Clears GPEs 0x09 and 0x0A.
        assert_eq!(pio_read(&gpe0, Register::Sts(0)), 0b0000_0010);
        assert_eq!(pio_read(&gpe0, Register::Sts(1)), 0b0100_0001);
        assert_eq!(pio_read(&gpe0, Register::En(0)), 0b0000_1100);
        assert_eq!(pio_read(&gpe0, Register::En(1)), 0b0100_0010);

        // STS is W1C. Writing to bits that are not set is a no-op.
        pio_write(&gpe0, Register::Sts(0), 0b0010_1000);
        pio_write(&gpe0, Register::Sts(1), 0b0000_0110);
        assert_eq!(pio_read(&gpe0, Register::Sts(0)), 0b0000_0010);
        assert_eq!(pio_read(&gpe0, Register::Sts(1)), 0b0100_0001);
        assert_eq!(pio_read(&gpe0, Register::En(0)), 0b0000_1100);
        assert_eq!(pio_read(&gpe0, Register::En(1)), 0b0100_0010);
    }

    #[test]
    fn pio_wide() {
        let (gpe0, _) = test_gpe0(4);

        // Initialize registers.
        pio_write(&gpe0, Register::En(0), 0xff);
        pio_write(&gpe0, Register::En(1), 0xff);

        [
            // GPEs in STS[0].
            Event::new(0x00, Trigger::Edge),
            Event::new(0x01, Trigger::Edge),
            // GPEs in STS[1].
            Event::new(0x0f, Trigger::Edge),
        ]
        .iter()
        .for_each(|gpe| {
            gpe0.trigger(*gpe);
        });

        // Read STS[0], STS[1], EN[0], and EN[1] in a single operation.
        let mut buf = [0; 4];
        gpe0.pio_rw(0, RWOp::Read(&mut ReadOp::from_buf(0, &mut buf)));
        assert_eq!(buf, [0b0000_0011, 0b1000_0000, 0xff, 0xff,]);

        // W1C both STS registers in a single operation.
        let buf = [0xff, 0xff];
        gpe0.pio_rw(0, RWOp::Write(&mut WriteOp::from_buf(0, &buf)));
        assert_eq!(pio_read(&gpe0, Register::Sts(0)), 0x00);
        assert_eq!(pio_read(&gpe0, Register::Sts(1)), 0x00);
        assert_eq!(pio_read(&gpe0, Register::En(0)), 0xff);
        assert_eq!(pio_read(&gpe0, Register::En(1)), 0xff);
    }

    #[test]
    fn trigger_edge_gpe() {
        // Check that GPEs are properly split between two blocks.
        {
            // gpe0 handles GPEs from 0..=7.
            let (gpe0, _) = test_gpe0(2);
            gpe0.trigger(Event::new(0x01, Trigger::Edge));
            assert_eq!(pio_read(&gpe0, Register::Sts(0)), 0b0000_0010);

            // gpe1 handles GPEs from 4..=11.
            let (gpe1, _) = test_new(2, 4);
            gpe1.trigger(Event::new(0x06, Trigger::Edge));
            assert_eq!(pio_read(&gpe1, Register::Sts(0)), 0b0000_0100);
        }

        // Simulate edge GPE being triggered.
        let (gpe, sci) = test_gpe0(2);
        let ev = Event::new(0x01, Trigger::Edge);

        // Guest enables the GPE and platform triggers the GPE.
        // SCI is asserted.
        pio_write(&gpe, Register::En(0), 0b0000_0010);
        gpe.trigger(ev);
        assert_eq!(pio_read(&gpe, Register::Sts(0)), 0b0000_0010);
        assert_eq!(pio_read(&gpe, Register::En(0)), 0b0000_0010);
        assert!(sci.is_asserted());

        // Guest disables and clears the GPE before handling it.
        // SCI is deasserted.
        pio_write(&gpe, Register::En(0), 0b0000_0000);
        pio_write(&gpe, Register::Sts(0), 0b0000_0010);
        assert_eq!(pio_read(&gpe, Register::Sts(0)), 0b0000_0000);
        assert_eq!(pio_read(&gpe, Register::En(0)), 0b0000_0000);
        assert!(!sci.is_asserted());

        // Guest handles the event and re-enables the GPE.
        pio_write(&gpe, Register::En(0), 0b0000_0010);
        assert_eq!(pio_read(&gpe, Register::Sts(0)), 0b0000_0000);
        assert_eq!(pio_read(&gpe, Register::En(0)), 0b0000_0010);
        assert!(!sci.is_asserted());

        // SCI should only have triggered once.
        assert_eq!(sci.rises(), 1);
    }

    #[test]
    fn set_level_gpe() {
        // Check that GPEs are properly split between two blocks.
        {
            // gpe0 handles GPEs from 0..=7.
            let (gpe0, _) = test_gpe0(2);
            gpe0.set_level(Event::new(0x01, Trigger::Level), true);
            assert_eq!(pio_read(&gpe0, Register::Sts(0)), 0b0000_0010);

            // gpe1 handles GPEs from 4..=11.
            let (gpe1, _) = test_new(2, 4);
            gpe1.set_level(Event::new(0x06, Trigger::Level), true);
            assert_eq!(pio_read(&gpe1, Register::Sts(0)), 0b0000_0100);
        }

        // Simulate level GPE.
        let (gpe, sci) = test_gpe0(2);
        let ev = Event::new(0x01, Trigger::Level);

        // Guest enables the GPE and platform asserts its line.
        // SCI is asserted.
        pio_write(&gpe, Register::En(0), 0b0000_0010);
        gpe.set_level(ev, true);
        assert_eq!(pio_read(&gpe, Register::Sts(0)), 0b0000_0010);
        assert_eq!(pio_read(&gpe, Register::En(0)), 0b0000_0010);
        assert!(sci.is_asserted());

        // Guest disables the GPE before handling it.
        //
        // For level-triggered events, the GPE is cleared only after the event
        // is handled.
        pio_write(&gpe, Register::En(0), 0b0000_0000);
        assert_eq!(pio_read(&gpe, Register::Sts(0)), 0b0000_0010);
        assert_eq!(pio_read(&gpe, Register::En(0)), 0b0000_0000);
        assert!(!sci.is_asserted());

        // Guest handles the event and clears the GPE, but the GPE line is
        // still asserted so the platform keeps STS set.
        pio_write(&gpe, Register::Sts(0), 0b0000_0010);
        assert_eq!(pio_read(&gpe, Register::Sts(0)), 0b0000_0010);
        assert_eq!(pio_read(&gpe, Register::En(0)), 0b0000_0000);
        assert!(!sci.is_asserted());

        // Guest re-enables the GPE after handling it.
        // SCI is re-asserted because the GPE line is still high.
        pio_write(&gpe, Register::En(0), 0b0000_0010);
        assert_eq!(pio_read(&gpe, Register::Sts(0)), 0b0000_0010);
        assert_eq!(pio_read(&gpe, Register::En(0)), 0b0000_0010);
        assert!(sci.is_asserted());
        assert_eq!(sci.falls(), 1);
        assert_eq!(sci.rises(), 2);

        // Platform deasserts the GPE line.
        // STS still high because only the guest can clear it.
        gpe.set_level(ev, false);
        assert_eq!(pio_read(&gpe, Register::Sts(0)), 0b0000_0010);
        assert_eq!(pio_read(&gpe, Register::En(0)), 0b0000_0010);
        assert!(sci.is_asserted());

        // Guest handles the event again and W1C the GPE.
        // SCI is deasserted this time because the GPE line is low.
        pio_write(&gpe, Register::En(0), 0b0000_0000);
        pio_write(&gpe, Register::Sts(0), 0b0000_0010);
        pio_write(&gpe, Register::En(0), 0b0000_0010);
        assert_eq!(pio_read(&gpe, Register::Sts(0)), 0b0000_0000);
        assert_eq!(pio_read(&gpe, Register::En(0)), 0b0000_0010);
        assert!(!sci.is_asserted());

        // SCI raised twice because the GPE line stayed high the first time the
        // guest handled the GPE.
        assert_eq!(sci.falls(), 2);
        assert_eq!(sci.rises(), 2);
    }

    #[test]
    fn reset() {
        let (gpe0, sci) = test_new(4, 2);

        let l_02 = Event::new(0x02, Trigger::Level);
        let e_09 = Event::new(0x09, Trigger::Edge);
        let l_0d = Event::new(0x0d, Trigger::Level);

        // Enable and activate test events.
        enable_gpes(&gpe0, &[l_02, e_09, l_0d]);
        assert_en(&gpe0, &[l_02, e_09, l_0d]);

        gpe0.trigger(e_09);
        gpe0.set_level(l_02, true);
        // Assert L0D to raise its level and set its STS bit, and deassert it
        // to lower its level but keep STS set.
        gpe0.set_level(l_0d, true);
        gpe0.set_level(l_0d, false);

        assert_sts(&gpe0, &[l_02, e_09, l_0d]);
        assert!(sci.is_asserted());

        // Reset register block.
        gpe0.reset();

        // Only L02 is still active because its level is still asserted.
        // All GPEs are disabled by the reset.
        assert_sts(&gpe0, &[l_02]);
        assert_en(&gpe0, &[]);
        assert!(!sci.is_asserted());
        assert_eq!(sci.falls(), 1);
    }

    #[test]
    fn acpica_sim() {
        let (gpe, sci) = test_gpe0(4);

        let e_01 = Event::new(0x01, Trigger::Edge);
        let l_04 = Event::new(0x04, Trigger::Level);
        let e_08 = Event::new(0x08, Trigger::Edge);
        let l_0d = Event::new(0x0d, Trigger::Level);

        let mut acpica = AcpicaSim::new(&gpe, false);

        // Add one edge-triggered event to each register.
        acpica.add_event(e_01, None);
        acpica.add_event(e_08, None);

        // Simulate a level-triggered event connected to a device that
        // deasserts its GPE line once its ACPI method notifies it that it ran.
        acpica.add_event(
            l_04,
            Some(Box::new(|| {
                gpe.set_level(l_04, false);
            })),
        );
        // Simulate a level-triggered event that never deasserts its GPE line.
        acpica.add_event(l_0d, None);

        // Initializes ACPICA. Simulates a guest booting.
        acpica.initialize();
        assert_eq!(pio_read(&gpe, Register::Sts(0)), 0x00);
        assert_eq!(pio_read(&gpe, Register::Sts(1)), 0x00);
        assert_eq!(pio_read(&gpe, Register::En(0)), 0b0001_0010);
        assert_eq!(pio_read(&gpe, Register::En(1)), 0b0010_0001);

        // Activate an edge and a level event.
        gpe.set_level(l_04, true);
        gpe.trigger(e_08);

        assert_eq!(pio_read(&gpe, Register::Sts(0)), 0b0001_0000);
        assert_eq!(pio_read(&gpe, Register::Sts(1)), 0b0000_0001);
        assert!(sci.is_asserted());

        // Notify ACPICA that SCI is asserted.
        acpica.interrupt();

        // ACPICA W1C the edge-triggered event (but not the level-triggered)
        // before running its method.
        assert_eq!(pio_read(&gpe, Register::Sts(0)), 0b0001_0000);
        assert_eq!(pio_read(&gpe, Register::Sts(1)), 0b0000_0000);
        // Active events are disabled until ACPICA runs their methods.
        assert_eq!(pio_read(&gpe, Register::En(0)), 0b0000_0010);
        assert_eq!(pio_read(&gpe, Register::En(1)), 0b0010_0000);

        // Run queued ACPI methods.
        acpica.run_queued_methods();

        // All events are cleared and enabled.
        assert_eq!(pio_read(&gpe, Register::Sts(0)), 0x00);
        assert_eq!(pio_read(&gpe, Register::Sts(1)), 0x00);
        assert_eq!(pio_read(&gpe, Register::En(0)), 0b0001_0010);
        assert_eq!(pio_read(&gpe, Register::En(1)), 0b0010_0001);

        // SCI raised only once for both events and is now deasserted.
        assert_eq!(sci.rises(), 1);
        assert!(!sci.is_asserted());

        // Activate all events.
        gpe.trigger(e_01);
        gpe.set_level(l_04, true);
        gpe.trigger(e_08);
        gpe.set_level(l_0d, true);

        assert_eq!(pio_read(&gpe, Register::Sts(0)), 0b0001_0010);
        assert_eq!(pio_read(&gpe, Register::Sts(1)), 0b0010_0001);
        assert!(sci.is_asserted());
        assert_eq!(sci.rises(), 2);

        // Notify ACPICA that SCI is asserted.
        acpica.interrupt();

        // Run the queued ACPI methods.
        // SCI reasserts because _L0D didn't deassert its line.
        acpica.run_queued_methods();
        assert_eq!(sci.rises(), 3);
        assert!(sci.is_asserted());

        // Deassert GPE line.
        // SCI remains asserted because the guest needs to W1C.
        gpe.set_level(l_0d, false);
        assert!(sci.is_asserted());

        // SCI was asserted again earlier, so run the ACPICA event handler.
        acpica.interrupt();
        acpica.run_queued_methods();

        // SCI can now deassert and did not assert again.
        assert_eq!(sci.rises(), 3);
        assert!(!sci.is_asserted());
    }

    #[test]
    fn migrate_edge() {
        let e_00 = Event::new(0x00, Trigger::Edge);
        let e_0f = Event::new(0x0f, Trigger::Edge);

        let payload = {
            let (gpe0, sci) = test_gpe0(4);

            // Enable and trigger edge GPE.
            enable_gpes(&gpe0, &[e_00, e_0f]);
            gpe0.trigger(e_00);
            gpe0.trigger(e_0f);

            assert!(sci.is_asserted());
            assert_eq!(sci.rises(), 1);
            assert_eq!(sci.falls(), 0);

            gpe0.export()
        };

        let (gpe0, sci) = test_gpe0(4);
        assert!(gpe0.import(payload).is_ok());

        assert_en(&gpe0, &[e_00, e_0f]);
        assert_sts(&gpe0, &[e_00, e_0f]);

        assert!(sci.is_asserted());
        assert_eq!(sci.rises(), 0);
        assert_eq!(sci.falls(), 0);
    }

    #[test]
    fn migrate_level() {
        let l_00 = Event::new(0x00, Trigger::Level);
        let l_02 = Event::new(0x02, Trigger::Level);
        let l_0f = Event::new(0x0f, Trigger::Level);

        let payload = {
            let (gpe0, sci) = test_gpe0(4);

            // Enable and assert level GPEs.
            enable_gpes(&gpe0, &[l_00, l_02, l_0f]);
            gpe0.set_level(l_00, true);
            gpe0.set_level(l_02, true);
            gpe0.set_level(l_0f, true);

            // Deassert L02 to simulate a GPE with STS set but with the device
            // GPE line deasserted.
            gpe0.set_level(l_02, false);

            assert!(sci.is_asserted());
            assert_eq!(sci.rises(), 1);
            assert_eq!(sci.falls(), 0);

            gpe0.export()
        };

        let (gpe0, sci) = test_gpe0(4);

        // Level-trigger GPE sources are expected to import their own state.
        //
        // Test a scenario where a device with a level-trigger GPE is imported
        // before the ACPI device, and another device is imported afterwards.
        gpe0.import_level(l_00, true);
        assert!(gpe0.import(payload).is_ok());
        gpe0.import_level(l_0f, true);
        gpe0.import_level(l_02, false);

        assert_en(&gpe0, &[l_00, l_02, l_0f]);
        assert_sts(&gpe0, &[l_00, l_02, l_0f]);
        assert_level(&gpe0, &[l_00, l_0f]);

        assert!(sci.is_asserted());
        assert_eq!(sci.rises(), 0);
        assert_eq!(sci.falls(), 0);
    }

    #[test]
    #[should_panic = "invalid GPE number"]
    fn panic_trigger_gpe_too_high() {
        // Register block with len=2 handles GPEs from 0..=7.
        let (gpe, _) = test_gpe0(2);
        gpe.trigger(Event::new(0x08, Trigger::Edge));
    }

    #[test]
    #[should_panic = "invalid GPE number"]
    fn panic_trigger_gpe_too_low() {
        // Register block with len=2 and base=4 handles GPEs from 4..=11.
        let (gpe1, _) = test_new(2, 4);
        gpe1.trigger(Event::new(0x03, Trigger::Edge));
    }

    #[test]
    #[should_panic = "expected STS register block of size"]
    fn panic_import_invalid_sts() {
        let payload =
            migrate::RegisterBlockV1 { sts: vec![0x00], en: vec![0x00, 0x00] };

        // Register block with len=4 expects payload to have vecs of len 2.
        let (gpe, _) = test_gpe0(4);
        gpe.import(payload).unwrap();
    }

    #[test]
    #[should_panic = "expected EN register block of size"]
    fn panic_import_invalid_en() {
        let payload =
            migrate::RegisterBlockV1 { sts: vec![0x00], en: vec![0x00, 0x00] };

        // Register block with len=2 expects payload to have vecs of len 1.
        let (gpe, _) = test_gpe0(2);
        gpe.import(payload).unwrap();
    }

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

    pub(crate) fn reg_index(gpe: Event, base: u8) -> usize {
        usize::from((gpe.number() - base) / 8)
    }

    pub(crate) fn reg_mask(gpe: Event, base: u8) -> u8 {
        1 << ((gpe.number() - base) % 8)
    }

    fn test_new(len: usize, base: u8) -> (RegisterBlock, Arc<TestPin>) {
        let pin = Arc::new(TestPin::new());
        (RegisterBlock::new(0x1234, len, base, pin.clone()), pin)
    }

    fn test_gpe0(len: usize) -> (RegisterBlock, Arc<TestPin>) {
        test_new(len, 0)
    }

    fn reg_offset(inner: &RegisterBlockInner, reg: Register) -> usize {
        match reg {
            Register::Sts(i) => i,
            Register::En(i) => inner.sts.len() + i,
        }
    }

    fn enable_gpes(reg_block: &RegisterBlock, gpes: &[Event]) {
        let len = reg_block.inner.lock().unwrap().en.len();
        let masks = generate_masks(reg_block, gpes, len);

        for (i, mask) in masks.iter().enumerate() {
            if *mask == 0 {
                continue;
            }
            let bits = pio_read(reg_block, Register::En(i));
            pio_write(reg_block, Register::En(i), bits | mask);
        }
    }

    fn assert_en(reg_block: &RegisterBlock, gpes: &[Event]) {
        let inner = reg_block.inner.lock().unwrap();
        let expected = generate_masks(reg_block, gpes, inner.en.len());

        for (i, got) in inner.en.iter().enumerate() {
            assert_eq!(
                *got, expected[i],
                "expected register EN[{i}] to be {:08b}, got: {got:08b}",
                expected[i],
            );
        }
    }

    fn assert_sts(reg_block: &RegisterBlock, gpes: &[Event]) {
        let inner = reg_block.inner.lock().unwrap();
        let expected = generate_masks(reg_block, gpes, inner.sts.len());

        for (i, got) in inner.sts.iter().enumerate() {
            assert_eq!(
                *got, expected[i],
                "expected register STS[{i}] to be {:08b}, got: {got:08b}",
                expected[i],
            );
        }
    }

    fn assert_level(reg_block: &RegisterBlock, gpes: &[Event]) {
        let inner = reg_block.inner.lock().unwrap();
        let expected = generate_masks(reg_block, gpes, inner.level.len());

        for (i, got) in inner.level.iter().enumerate() {
            assert_eq!(
                *got, expected[i],
                "expected LEVEL[{i}] to be {:08b}, got: {got:08b}",
                expected[i],
            );
        }
    }

    fn generate_masks(
        reg_block: &RegisterBlock,
        gpes: &[Event],
        len: usize,
    ) -> Vec<u8> {
        let mut masks = vec![0_u8; len];
        let base = *reg_block.gpe_range.start();
        for gpe in gpes {
            let reg_idx = reg_index(*gpe, base);
            let gpe_mask = reg_mask(*gpe, base);
            masks[reg_idx] |= gpe_mask;
        }

        masks
    }
}

#[usdt::provider(provider = "propolis")]
mod probes {
    fn gpe_read(addr: u16, reg: &str, idx: u8, value: u8) {}
    fn gpe_write(addr: u16, reg: &str, idx: u8, value: u8, old: u8, new: u8) {}
    fn gpe_sts_raise(addr: u16, bit: u8, sts: u8, en: u8) {}
    fn gpe_set_level(addr: u16, bit: u8, sts: u8, en: u8, level: u8) {}
    fn gpe_update_sci(addr: u16, state: u8) {}
    fn gpe_reset(addr: u16) {}
}
