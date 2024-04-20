// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! 16550 UART
//!
//! Host -> Device Data Path:
//! The host writes data to the UART via its Transmitter Holding Register (THR),
//! which is backed by tx_fifo. After this data is received out of the THR, the
//! UART will raise the Transmitter Holding Register Empty interrupt, which
//! notifies the host that it can write more data.

use std::collections::VecDeque;
use std::convert::AsRef;

use crate::migrate::MigrateStateError;

use serde::{Deserialize, Serialize};
use strum::{AsRefStr, FromRepr};

#[usdt::provider(provider = "propolis")]
mod probes {
    fn uart_reg_read(offset: u8, is_dlab: u8, val: u8) {}
    fn uart_reg_write(offset: u8, is_dlab: u8, data: u8) {}
    fn uart_tx_discard(data: u8) {}
    fn uart_ign_write(offset: u8, is_dlab: u8, data: u8) {}
    fn uart_ign_read(offset: u8, is_dlab: u8) {}
}

pub struct Uart {
    reg_intr_enable: IntrEnaReg,
    reg_intr_ident: IntrIdentReg,
    // TODO: add FIFO support
    // reg_fifo_ctrl: u8,
    reg_line_ctrl: LineCtrlReg,
    reg_line_status: LineStatusReg,
    reg_modem_ctrl: ModemCtrlReg,
    reg_modem_status: u8,
    reg_scratch: u8,
    reg_div_low: u8,
    reg_div_high: u8,

    /// Transmitter-Holding-Register-Empty interrupt status
    ///
    /// Since reading IIR while THRE is asserted will clear the THRE interrupt,
    /// but leave the associated THRE/TEMT bits asserted in RLS, we must track
    /// the interrupt state seperately.
    thre_intr: bool,
    intr_pin: bool,

    rx_fifo: Fifo,
    tx_fifo: Fifo,
}
impl Uart {
    pub fn new() -> Self {
        Uart {
            reg_intr_enable: IntrEnaReg::default(),
            reg_intr_ident: IntrIdentReg::default(),
            // reg_fifo_ctrl: 0,
            reg_line_ctrl: LineCtrlReg::default(),
            reg_line_status: LineStatusReg::default(),
            reg_modem_ctrl: ModemCtrlReg::default(),
            reg_modem_status: 0,
            reg_scratch: 0,
            reg_div_low: 0,
            reg_div_high: 0,

            thre_intr: false,
            intr_pin: false,
            // TODO: Don't deal with "real" sized fifos for now
            rx_fifo: Fifo::new(1),
            tx_fifo: Fifo::new(1),
        }
    }
    /// Read UART register
    pub fn reg_read(&mut self, offset: u8) -> u8 {
        let is_dlab = self.is_dlab();

        let val = match UartReg::for_read(offset, is_dlab) {
            Some(UartReg::DivisorLow) => self.reg_div_low,
            Some(UartReg::DivisorHigh) => self.reg_div_high,

            Some(UartReg::RecvHold) => {
                if let Some(d) = self.rx_fifo.read() {
                    self.update_dr();
                    self.update_isr();
                    d
                } else {
                    0
                }
            }
            Some(UartReg::IntrEnable) => self.reg_intr_enable.bits(),
            Some(UartReg::IntrIdent) => {
                let val = self.reg_intr_ident;
                if let Some(IntrIdent::THRE) = val.get_intr() {
                    // Reading the ISR can clear the THRE interrupt.
                    // The flag will remain in RLS, though.
                    self.thre_intr = false;
                    self.update_isr();
                }
                val.bits()
            }
            Some(UartReg::LineCtrl) => self.reg_line_ctrl.bits(),
            Some(UartReg::ModemCtrl) => self.reg_modem_ctrl.bits(),
            Some(UartReg::LineStatus) => {
                let val = self.reg_line_status;
                self.reg_line_status.remove(LineStatusReg::OE);
                self.update_isr();

                val.bits()
            }
            Some(UartReg::ModemStatus) => self.reg_modem_status,
            Some(UartReg::Scratch) => self.reg_scratch,
            Some(reg) => {
                assert!(!reg.is_readable());
                panic!(
                    "uart reg {} should not decode to be readable",
                    reg.as_ref()
                );
            }
            None => {
                probes::uart_ign_read!(|| (offset, u8::from(is_dlab)));
                0
            }
        };

        probes::uart_reg_read!(|| (offset, u8::from(is_dlab), val));

        val
    }

    /// Write UART register
    pub fn reg_write(&mut self, offset: u8, data: u8) {
        let is_dlab = self.is_dlab();

        probes::uart_reg_write!(|| (offset, u8::from(is_dlab), data));
        match UartReg::for_write(offset, is_dlab) {
            Some(UartReg::DivisorLow) => {
                self.reg_div_low = data;
            }
            Some(UartReg::DivisorHigh) => {
                self.reg_div_high = data;
            }
            Some(UartReg::TransmitHold) => {
                if !self.is_loopback() {
                    if !self.tx_fifo.write(data) {
                        // There is no error flag for when the TX buffer is
                        // overrun, but we can at least fire a probe.
                        probes::uart_tx_discard!(|| { data });
                    }
                    self.set_thre(false);
                } else {
                    if !self.rx_fifo.write(data) {
                        self.reg_line_status.insert(LineStatusReg::OE);
                    }
                    self.update_dr();
                    self.set_thre(true);
                }
            }
            Some(UartReg::IntrEnable) => {
                let old = self.reg_intr_enable;
                let new = IntrEnaReg::from_bits_truncate(data);
                self.reg_intr_enable = new;
                // Although not specified in the datasheet, some consumers
                // expect a THRE interrupt to be raised when toggling that on in
                // IER.
                if !old.contains(IntrEnaReg::ETBEI)
                    && new.contains(IntrEnaReg::ETBEI)
                {
                    if self.tx_fifo.is_empty() {
                        self.thre_intr = true
                    }
                }
                self.update_isr();
            }
            Some(UartReg::FifoCtrl) => {
                // TODO: add FIFO support
                // self.reg_fifo_ctrl = ?;
            }
            Some(UartReg::LineCtrl) => {
                // Accept any line control configuration.
                // We don't pay heed to anything but DLAB
                self.reg_line_ctrl = LineCtrlReg::from_bits_retain(data);
            }
            Some(UartReg::ModemCtrl) => {
                self.reg_modem_ctrl = ModemCtrlReg::from_bits_truncate(data);
            }
            Some(UartReg::Scratch) => {
                self.reg_scratch = data;
            }
            Some(reg) => {
                assert!(!reg.is_writable());
                panic!(
                    "uart reg {} should not decode to be writable",
                    reg.as_ref()
                );
            }
            None => {
                probes::uart_ign_read!(|| (offset, u8::from(is_dlab), data));
            }
        }
    }
    /// Read data transmitted from the uart
    pub fn data_read(&mut self) -> Option<u8> {
        if let Some(d) = self.tx_fifo.read() {
            self.set_thre(self.tx_fifo.is_empty());
            Some(d)
        } else {
            None
        }
    }
    /// Write data to be received by the uart
    pub fn data_write(&mut self, data: u8) -> bool {
        if self.is_loopback() {
            // Per the datasheet, the serial input pin is disconnected.
            // Simply discard all incoming data.
            true
        } else {
            let res = self.rx_fifo.write(data);
            self.update_dr();
            self.update_isr();
            res
        }
    }
    pub fn intr_state(&self) -> bool {
        self.intr_pin
    }
    pub fn is_readable(&self) -> bool {
        !self.tx_fifo.is_empty()
    }
    pub fn is_writable(&self) -> bool {
        self.is_loopback() || !self.rx_fifo.is_full()
    }

    pub fn reset(&mut self) {
        self.reg_intr_enable = IntrEnaReg::default();
        self.reg_intr_ident = IntrIdentReg::default();
        // self.reg_fifo_ctrl = 0;
        self.reg_line_ctrl = LineCtrlReg::default();
        self.reg_line_status = LineStatusReg::default();
        self.reg_modem_ctrl = ModemCtrlReg::default();
        self.reg_modem_status = 0;
        self.reg_scratch = 0;
        self.reg_div_low = 0;
        self.reg_div_high = 0;

        self.thre_intr = false;
        self.intr_pin = false;

        self.rx_fifo.reset();
        self.tx_fifo.reset();
    }

    #[inline(always)]
    fn is_dlab(&self) -> bool {
        self.reg_line_ctrl.contains(LineCtrlReg::DLAB)
    }
    #[inline(always)]
    fn is_loopback(&self) -> bool {
        self.reg_modem_ctrl.contains(ModemCtrlReg::LOOP)
    }

    fn next_intr(&self) -> Option<IntrIdent> {
        if self.reg_intr_enable.contains(IntrEnaReg::ELSI)
            && self.reg_line_status.contains(LineStatusReg::OE)
        {
            // This ignores Parity Error, Framing Error, and Break
            Some(IntrIdent::RLS)
        } else if self.reg_intr_enable.contains(IntrEnaReg::ERBFI)
            && self.reg_line_status.contains(LineStatusReg::DR)
        {
            Some(IntrIdent::DR)
        } else if self.reg_intr_enable.contains(IntrEnaReg::ETBEI)
            && self.thre_intr
        {
            Some(IntrIdent::THRE)
        } else if self.reg_intr_enable.contains(IntrEnaReg::EDSSI)
            && self.reg_modem_status != 0
        {
            // This ignores that MSR is fixed to 0
            Some(IntrIdent::MDM)
        } else {
            None
        }
    }

    fn update_isr(&mut self) {
        let new_isr = self.next_intr();
        self.reg_intr_ident.set_intr(new_isr);
        self.intr_pin = new_isr.is_some();
    }

    fn set_thre(&mut self, state: bool) {
        self.reg_line_status
            .set(LineStatusReg::THRE | LineStatusReg::TEMT, state);
        if self.thre_intr != state {
            self.thre_intr = state;
        }
        self.update_isr();
    }
    fn update_dr(&mut self) {
        self.reg_line_status.set(LineStatusReg::DR, !self.rx_fifo.is_empty())
    }

    pub(super) fn export(&self) -> migrate::Uart16550V1 {
        migrate::Uart16550V1 {
            intr_enable: self.reg_intr_enable.bits(),
            intr_status: self.reg_intr_ident.bits(),
            line_ctrl: self.reg_line_ctrl.bits(),
            line_status: self.reg_line_status.bits(),
            modem_ctrl: self.reg_modem_ctrl.bits(),
            modem_status: self.reg_modem_status,
            scratch: self.reg_scratch,
            div_low: self.reg_div_low,
            div_high: self.reg_div_high,
            thre_state: self.thre_intr,
            rx_fifo: self.rx_fifo.buf.clone().into(),
            tx_fifo: self.tx_fifo.buf.clone().into(),
        }
    }
    pub(super) fn import(
        &mut self,
        state: &migrate::Uart16550V1,
    ) -> Result<(), MigrateStateError> {
        if self.rx_fifo.len < state.rx_fifo.len() {
            return Err(MigrateStateError::ImportFailed(format!(
                "RX FIFO contents too long: {}",
                state.rx_fifo.len()
            )));
        }
        if self.tx_fifo.len < state.tx_fifo.len() {
            return Err(MigrateStateError::ImportFailed(format!(
                "TX FIFO contents too long: {}",
                state.rx_fifo.len()
            )));
        }

        self.reg_intr_enable =
            IntrEnaReg::from_bits_truncate(state.intr_enable);
        self.reg_intr_ident =
            IntrIdentReg::from_bits_truncate(state.intr_status);
        self.reg_line_ctrl = LineCtrlReg::from_bits_retain(state.line_ctrl);
        self.reg_line_status =
            LineStatusReg::from_bits_truncate(state.line_status);
        self.reg_modem_ctrl =
            ModemCtrlReg::from_bits_truncate(state.modem_ctrl);
        self.reg_modem_status = state.modem_status;
        self.reg_scratch = state.scratch;
        self.reg_div_low = state.div_low;
        self.reg_div_high = state.div_high;
        self.thre_intr = state.thre_state;
        self.rx_fifo.buf = state.rx_fifo.clone().into();
        self.tx_fifo.buf = state.tx_fifo.clone().into();

        // synthesize interrupt pin state like update_isr()
        self.intr_pin = self.reg_intr_ident.get_intr().is_some();

        Ok(())
    }
}

#[derive(Deserialize, Serialize, Clone)]
pub struct Fifo {
    len: usize,
    buf: VecDeque<u8>,
}

impl Fifo {
    fn new(max_len: usize) -> Self {
        Fifo { len: max_len, buf: VecDeque::with_capacity(max_len) }
    }
    fn write(&mut self, data: u8) -> bool {
        if self.buf.len() < self.len {
            self.buf.push_back(data);
            true
        } else {
            false
        }
    }
    fn read(&mut self) -> Option<u8> {
        self.buf.pop_front()
    }
    fn reset(&mut self) {
        self.buf.clear();
    }
    fn is_empty(&self) -> bool {
        self.buf.len() == 0
    }
    fn is_full(&self) -> bool {
        self.buf.len() == self.len
    }
}

pub mod migrate {
    use crate::migrate::*;

    use serde::{Deserialize, Serialize};

    #[derive(Deserialize, Serialize)]
    pub struct Uart16550V1 {
        pub intr_enable: u8,
        pub intr_status: u8,
        pub line_ctrl: u8,
        pub line_status: u8,
        pub modem_ctrl: u8,
        pub modem_status: u8,
        pub scratch: u8,
        pub div_low: u8,
        pub div_high: u8,
        pub thre_state: bool,
        pub rx_fifo: Vec<u8>,
        pub tx_fifo: Vec<u8>,
    }
    impl Schema<'_> for Uart16550V1 {
        fn id() -> SchemaId {
            ("uart-16550", 1)
        }
    }
}

bitflags! {
    /// Interrupt Enable Register (IER)
    #[derive(Default, Copy, Clone)]
    struct IntrEnaReg: u8 {
        /// Receiver Data Available interrupt enable
        const ERBFI = 1 << 0;
        /// Transmit Holding Register Empty interrupt enable
        const ETBEI = 1 << 1;
        /// Receiver Line Status interrupt enable
        const ELSI = 1 << 2;
        /// Modem Status interrupt
        const EDSSI = 1 << 3;
    }

    /// Interrupt Identification Register (IIR)
    #[derive(Copy, Clone)]
    struct IntrIdentReg: u8 {
        /// Interrupt Pending (clear when interrupt pending)
        const NOPEND = 1;

        /// Mask of potential interrupt IDs
        const INTID = 0b1110;
    }

    /// FIFO Control Register (FCR)
    #[derive(Default, Copy, Clone)]
    struct FifoCtrlReg: u8 {
        /// enable transmitter/receive FIFOs
        const ENA = 1 << 0;
        /// clear bytes and count in receiver FIFO
        const RXRST = 1 << 1;
        /// clear bytes and count in transmit FIFO
        const TXRST = 1 << 2;
        const DMAMD = 1 << 3;
        const TRGR = 0b11000000;
    }

    /// Modem Control Register (MCR)
    #[derive(Default, Copy, Clone)]
    struct ModemCtrlReg: u8 {
        /// Loopback enabled
        const LOOP = 1 << 4;
    }

    /// Line Status Register (LSR)
    #[derive(Copy, Clone)]
    struct LineStatusReg: u8 {
        /// Data Ready
        const DR = 1 << 0;
        /// Overrun Error
        const OE = 1 << 1;
        /// Transmit Hold Register Empty
        const THRE = 1 << 5;
        /// Transmitter Empty
        const TEMT = 1 << 6;
    }

    /// Line Control Register (LSR)
    #[derive(Default, Copy, Clone)]
    struct LineCtrlReg: u8 {
        /// Word Length Select (0b11 = 8 bits)
        const WLS = 0b11;
        /// Stop Bits
        const STB = 1 << 2;
        /// Parent Enable
        const PEN = 1 << 3;
        /// Even Parity Select
        const EPS = 1 << 4;
        /// Stick Parity
        const SP = 1 << 5;
        /// Break Condition
        const BC = 1 << 6;
        /// Divisor Latch Access Bit
        const DLAB = 1 << 7;
    }
}

impl Default for LineStatusReg {
    fn default() -> Self {
        LineStatusReg::TEMT | LineStatusReg::THRE
    }
}

impl IntrIdentReg {
    fn set_intr(&mut self, intr_id: Option<IntrIdent>) {
        // Clear any existing interrupt bits
        self.remove(IntrIdentReg::INTID);
        if let Some(id) = intr_id {
            self.0 .0 |= id as u8;
            self.remove(IntrIdentReg::NOPEND);
        } else {
            self.insert(IntrIdentReg::NOPEND);
        }
    }
    fn get_intr(&self) -> Option<IntrIdent> {
        if self.contains(IntrIdentReg::NOPEND) {
            None
        } else {
            IntrIdent::from_repr(self.intersection(IntrIdentReg::INTID).bits())
        }
    }
}

impl Default for IntrIdentReg {
    fn default() -> Self {
        IntrIdentReg::NOPEND
    }
}

#[repr(u8)]
#[derive(Copy, Clone, FromRepr)]
enum IntrIdent {
    /// MODEM Status, priority 4 (lowest)
    MDM = 0b0000,
    /// Transmitter Hold Register Empty, priority 3
    THRE = 0b0010,
    /// Data Ready, priority 2
    DR = 0b0100,
    /// Receiver Line Status, priority 1 (highest)
    RLS = 0b0110,
    /// Character Timeout, priority 2
    CTMO = 0b1100,
}

#[derive(Clone, Copy, AsRefStr)]
enum UartReg {
    /// Receiver Holding Register (RHR), RO
    RecvHold,
    /// Transmitter Holding Register (THR), WO
    TransmitHold,
    /// Interrupt Enable Register (IER), RW
    IntrEnable,
    /// Interrupt Identification Register (IIR), RO
    IntrIdent,
    /// FIFO Control Register (FCR), WO
    FifoCtrl,
    /// Line Control Register (LCR), RW
    LineCtrl,
    /// Modem Control Register (MCR), RW
    ModemCtrl,
    /// Line Status Register (LCR), RO
    LineStatus,
    /// Modem Status Register (MSR), RO
    ModemStatus,
    /// Scratch Register (SPR), RW
    Scratch,
    /// Divisor Latch LSB (DLL), RW
    DivisorLow,
    /// Divisor Latch MSB (DLH), RW
    DivisorHigh,
}

impl UartReg {
    const fn for_write(off: u8, dlab_status: bool) -> Option<Self> {
        match (off, dlab_status) {
            (0, true) => Some(Self::DivisorLow),
            (0, false) => Some(Self::TransmitHold),
            (1, true) => Some(Self::DivisorHigh),
            (1, false) => Some(Self::IntrEnable),
            (2, _) => Some(Self::FifoCtrl),
            (3, _) => Some(Self::LineCtrl),
            (4, _) => Some(Self::ModemCtrl),
            (5, _) => None,
            (6, _) => None,
            (7, _) => Some(Self::Scratch),
            _ => None,
        }
    }

    const fn for_read(off: u8, dlab_status: bool) -> Option<Self> {
        match (off, dlab_status) {
            (0, true) => Some(Self::DivisorLow),
            (0, false) => Some(Self::RecvHold),
            (1, true) => Some(Self::DivisorHigh),
            (1, false) => Some(Self::IntrEnable),
            (2, _) => Some(Self::IntrIdent),
            (3, _) => Some(Self::LineCtrl),
            (4, _) => Some(Self::ModemCtrl),
            (5, _) => Some(Self::LineStatus),
            (6, _) => Some(Self::ModemStatus),
            (7, _) => Some(Self::Scratch),
            _ => None,
        }
    }

    const fn is_readable(self) -> bool {
        match self {
            UartReg::RecvHold
            | UartReg::IntrEnable
            | UartReg::IntrIdent
            | UartReg::LineCtrl
            | UartReg::ModemCtrl
            | UartReg::LineStatus
            | UartReg::ModemStatus
            | UartReg::Scratch
            | UartReg::DivisorLow
            | UartReg::DivisorHigh => true,
            _ => false,
        }
    }

    const fn is_writable(self) -> bool {
        match self {
            UartReg::TransmitHold
            | UartReg::IntrEnable
            | UartReg::FifoCtrl
            | UartReg::LineCtrl
            | UartReg::ModemCtrl
            | UartReg::Scratch
            | UartReg::DivisorLow
            | UartReg::DivisorHigh => true,
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {

    mod bits {
        #![allow(unused)]

        // Register offsets from base
        pub const REG_RHR: u8 = 0b000; // Receiver Buffer Register (RO)
        pub const REG_THR: u8 = 0b000; // Transmitter Holding Register (WO)
        pub const REG_IER: u8 = 0b001; // Interrupt Enable Register (RW)
        pub const REG_ISR: u8 = 0b010; // Interrupt Ident Register (RO)
        pub const REG_FCR: u8 = 0b010; // FIFO Control Register (WO)
        pub const REG_LCR: u8 = 0b011; // Line Control Register (RW)
        pub const REG_MCR: u8 = 0b100; // Modem Control Register (RW)
        pub const REG_LSR: u8 = 0b101; // Line Status Register (RO)
        pub const REG_MSR: u8 = 0b110; // Modem Status Register (RO)
        pub const REG_SPR: u8 = 0b111; // Scratch Register (RW)
        pub const REG_DLL: u8 = 0b000; // Divisor Latch LSB (RW when DLAB=1)
        pub const REG_DLH: u8 = 0b001; // Divisor Latch MSB (RW when DLAB=1)

        // Interrupt Enable Register (IER) bits
        pub const IER_ERBFI: u8 = 1 << 0; // enable received-data-available-intr
        pub const IER_ETBEI: u8 = 1 << 1; // enable xmit-holding-reg-empty-intr
        pub const IER_ELSI: u8 = 1 << 2; // enable receiver-line-status
        pub const IER_EDSSI: u8 = 1 << 3; // enable modem-status-intr

        // Possible values of Interrupt Identification Register
        pub const ISRC_NONE: u8 = 0b0001; // no interrupt
        pub const ISRC_RLS: u8 = 0b0110; // receiver line status
        pub const ISRC_DR: u8 = 0b0100; // data ready
        pub const ISRC_TMO: u8 = 0b1100; // character timeout
        pub const ISRC_THRE: u8 = 0b0010; // transmitter holding register empty
        pub const ISRC_MDM: u8 = 0b0000; // modem status

        // FIFO Control Register (FCR) bits
        pub const FCR_ENA: u8 = 1 << 0; // enable xmit/receive FIFOs
        pub const FCR_RXRST: u8 = 1 << 1; // clear bytes/count in recv FIFO
        pub const FCR_TXRST: u8 = 1 << 2; // clear bytes/count in xmit FIFO
        pub const FCR_DMAMD: u8 = 1 << 3;
        pub const FCR_TRGR: u8 = 0b11000000;

        // Modem Control Register (MCR) bits
        pub const MCR_LOOP: u8 = 1 << 4; // loopback

        // Line Status Register (LSR) bits
        pub const LSR_DR: u8 = 1 << 0; // Data Ready
        pub const LSR_OE: u8 = 1 << 1; // Overrun Error
        pub const LSR_THRE: u8 = 1 << 5; // THRE indicator
        pub const LSR_TEMT: u8 = 1 << 6; // Transmitter Empty indicator
        pub const LCR_DLAB: u8 = 0b10000000; // Divisor Latch Access Bit

        pub const MASK_PCD: u8 = 0b00001111;
        pub const MASK_MCR: u8 = 0b00011111;
        pub const MASK_IER: u8 = 0b00001111;
        pub const MASK_FCR: u8 = 0b11001111;
        pub const MASK_ISRC: u8 = 0b00001111;
    }

    use super::*;
    use bits::*;

    #[test]
    fn reset_state() {
        let mut uart = Uart::new();
        assert_eq!(uart.reg_read(REG_IER), 0);
        assert_eq!(uart.reg_read(REG_ISR), 1);
        // TI datasheet notes the state of this register, despite it being WO
        // assert_eq!(uart.reg_fifo_ctrl, 0);
        assert_eq!(uart.reg_read(REG_LCR), 0);
        assert_eq!(uart.reg_read(REG_MCR), 0);
        assert_eq!(uart.reg_read(REG_LSR), 0b01100000);
    }
    #[test]
    fn intr_thre_on_etbei_toggle() {
        let mut uart = Uart::new();
        // start with no interrupts enabled, none should be asserted
        uart.reg_write(REG_IER, 0);
        assert_eq!(uart.reg_read(REG_LSR) & LSR_THRE, LSR_THRE);
        assert_eq!(uart.reg_read(REG_ISR) & MASK_ISRC, ISRC_NONE);
        assert_eq!(uart.intr_state(), false);
        // enable THRE interrupt
        uart.reg_write(REG_IER, IER_ETBEI);
        assert_eq!(uart.reg_read(REG_LSR) & LSR_THRE, LSR_THRE);
        assert_eq!(uart.intr_state(), true);
        assert_eq!(uart.reg_read(REG_ISR) & MASK_ISRC, ISRC_THRE);
        // after reading ISR, THRE interrupt should deassert
        assert_eq!(uart.intr_state(), false);
        assert_eq!(uart.reg_read(REG_ISR) & MASK_ISRC, ISRC_NONE);
        // should still be present in LSR, though
        assert_eq!(uart.reg_read(REG_LSR) & LSR_THRE, LSR_THRE);
    }
    #[test]
    fn intr_dr_on_incoming() {
        let mut uart = Uart::new();
        let tval = 0x20;

        uart.reg_write(REG_IER, IER_ERBFI);
        assert_eq!(uart.intr_state(), false);
        assert_eq!(uart.reg_read(REG_ISR) & MASK_ISRC, ISRC_NONE);
        uart.data_write(tval);
        assert_eq!(uart.intr_state(), true);
        assert_eq!(uart.reg_read(REG_ISR) & MASK_ISRC, ISRC_DR);
        assert_eq!(uart.reg_read(REG_RHR), tval);
        assert_eq!(uart.intr_state(), false);
        assert_eq!(uart.reg_read(REG_ISR) & MASK_ISRC, ISRC_NONE);
    }
    #[test]
    fn intr_thre_on_outgoing() {
        let mut uart = Uart::new();
        let tval = 0x20;

        uart.reg_write(REG_IER, 0);
        assert_eq!(uart.intr_state(), false);
        uart.reg_write(REG_THR, tval);
        uart.reg_write(REG_IER, IER_ETBEI);
        assert_eq!(uart.intr_state(), false);
        assert_eq!(uart.reg_read(REG_ISR) & MASK_ISRC, ISRC_NONE);
        assert_eq!(uart.data_read(), Some(tval));
        assert_eq!(uart.intr_state(), true);
        assert_eq!(uart.reg_read(REG_ISR) & MASK_ISRC, ISRC_THRE);
        // cleared after read of ISR
        assert_eq!(uart.intr_state(), false);
        assert_eq!(uart.reg_read(REG_ISR) & MASK_ISRC, ISRC_NONE);
    }
    #[test]
    fn safe_read_write_all() {
        let mut uart = Uart::new();

        for i in 0..=7 {
            let _ = uart.reg_read(i);
        }
        for i in 0..=7 {
            uart.reg_write(i, 0xff);
        }
        // With DLAB=1 now true, make sure the divisor registers are fine
        let _ = uart.reg_read(0);
        let _ = uart.reg_read(1);
        let _ = uart.reg_write(0, 0xff);
        let _ = uart.reg_write(1, 0xff);
    }
    #[test]
    fn interrupt_codes() {
        let mut uart = Uart::new();

        // Enable interrupts we're going to test
        uart.reg_write(REG_IER, IER_ERBFI | IER_ETBEI | IER_ELSI | IER_EDSSI);

        // Since no data has been sent, the TX register is empty
        assert_eq!(uart.reg_read(REG_ISR), ISRC_THRE);

        // Since triggering overflow requires us to use loopback mode (since the
        // data_write() path refuses to allow overflow), configure the uart for
        // loopback.  That state can be used for the data-ready intr as well.
        uart.reg_write(REG_MCR, MCR_LOOP);

        // Loop back data to assert the data-ready interrupt
        let rval = 0x20;
        uart.reg_write(REG_THR, rval);
        // data-ready interrupt should take precedence
        assert_eq!(uart.reg_read(REG_ISR), ISRC_DR);

        // Now overrun the read register
        uart.reg_write(REG_THR, rval);
        // receiver-line-status interrupt should take precedence
        assert_eq!(uart.reg_read(REG_ISR), ISRC_RLS);

        // Read RLS to clear RLS intr
        assert!((uart.reg_read(REG_LSR) & LSR_OE) != 0);
        assert_eq!(uart.reg_read(REG_ISR), ISRC_DR);

        // Read pending data to clear DR intr
        assert_eq!(uart.reg_read(REG_RHR), rval);
        assert_eq!(uart.reg_read(REG_ISR), ISRC_THRE);

        // Clear loopback mode and queue outgoing data in TX register
        uart.reg_write(REG_MCR, 0);
        let tval = 0x40;
        uart.reg_write(REG_THR, tval);
        assert_eq!(uart.reg_read(REG_ISR), ISRC_NONE);
        assert_eq!(uart.data_read(), Some(tval));
    }
}
