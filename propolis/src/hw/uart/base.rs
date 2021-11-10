use std::collections::VecDeque;

use bits::*;

pub struct Uart {
    reg_intr_enable: u8,
    reg_intr_status: u8,
    // TODO: add FIFO support
    // reg_fifo_ctrl: u8,
    reg_line_ctrl: u8,
    reg_line_status: u8,
    reg_modem_ctrl: u8,
    reg_modem_status: u8,
    reg_scratch: u8,
    reg_div_low: u8,
    reg_div_high: u8,

    thre_intr: bool,
    intr_pin: bool,

    rx_fifo: Fifo,
    tx_fifo: Fifo,
}
impl Uart {
    pub fn new() -> Self {
        Uart {
            reg_intr_enable: 0,
            reg_intr_status: ISRC_NONE,
            // reg_fifo_ctrl: 0,
            reg_line_ctrl: 0,
            reg_line_status: LSR_THRE | LSR_TEMT,
            reg_modem_ctrl: 0,
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
        match (offset, self.is_dlab()) {
            (REG_RHR, false) => {
                if let Some(d) = self.rx_fifo.read() {
                    self.update_dr();
                    self.update_isr();
                    d
                } else {
                    0u8
                }
            }
            (REG_IER, false) => self.reg_intr_enable,
            (REG_ISR, _) => {
                let isr = self.reg_intr_status;
                if isr & MASK_ISRC == ISRC_THRE {
                    // Reading the ISR can clear the THRE interrupt.
                    // The flag will remain in RLS, though.
                    self.thre_intr = false;
                    self.update_isr();
                }
                isr
            }
            (REG_LCR, _) => self.reg_line_ctrl,
            (REG_MCR, _) => self.reg_modem_ctrl,
            (REG_LSR, _) => self.reg_line_status,
            (REG_MSR, _) => self.reg_modem_status,
            (REG_SPR, _) => self.reg_scratch,
            // DLAB=1
            (REG_DLL, true) => self.reg_div_low,
            (REG_DLH, true) => self.reg_div_high,
            _ => {
                panic!();
            }
        }
    }
    /// Write UART register
    pub fn reg_write(&mut self, offset: u8, data: u8) {
        match (offset, self.is_dlab()) {
            (REG_THR, false) => {
                if !self.is_loopback() {
                    self.tx_fifo.write(data);
                    self.set_thre(false);
                } else {
                    if !self.rx_fifo.write(data) {
                        self.reg_line_status |= LSR_OE;
                    }
                    self.update_dr();
                    self.set_thre(true);
                }
            }
            (REG_IER, false) => {
                let old = self.reg_intr_enable;
                self.reg_intr_enable = data;
                // Although not specified in the datasheet, some consumers expect a THRE
                // interrupt to be raised when toggling that on in IER.
                if old & IER_ETBEI == 0 && data & IER_ETBEI != 0 {
                    if self.tx_fifo.is_empty() {
                        self.thre_intr = true
                    }
                }
                self.update_isr();
            }
            (REG_FCR, _) => {
                // TODO: add FIFO support
                // self.reg_fifo_ctrl = ?;
            }
            (REG_LCR, _) => {
                // Accept any line control configuration.
                // We don't pay heed to anything but DLAB
                self.reg_line_ctrl = data;
            }
            (REG_MCR, _) => {
                self.reg_modem_ctrl = data & MASK_MCR;
            }
            (REG_LSR, _) => {
                // ignore writes to read-only line-status
            }
            (REG_MSR, _) => {
                // ignore writes to read-only modem-status
            }
            (REG_SPR, _) => {
                self.reg_scratch = data;
            }
            // DLAB=1
            (REG_DLL, true) => {
                self.reg_div_low = data;
            }
            (REG_DLH, true) => {
                self.reg_div_high = data;
            }
            _ => {
                panic!();
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
        self.reg_intr_enable = 0;
        self.reg_intr_status = ISRC_NONE;
        // self.reg_fifo_ctrl = 0;
        self.reg_line_ctrl = 0;
        self.reg_line_status = LSR_THRE | LSR_TEMT;
        self.reg_modem_ctrl = 0;
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
        (self.reg_line_ctrl & LCR_DLAB) != 0
    }
    #[inline(always)]
    fn is_loopback(&self) -> bool {
        (self.reg_modem_ctrl & MCR_LOOP) != 0
    }

    fn next_intr(&self) -> u8 {
        if self.reg_intr_enable & IER_ELSI != 0
            && self.reg_line_status & LSR_OE != 0
        {
            // This ignores Parity Error, Framing Error, and Break
            ISRC_RLS
        } else if self.reg_intr_enable & IER_ERBFI != 0
            && self.reg_line_status & LSR_DR != 0
        {
            ISRC_DR
        } else if self.reg_intr_enable & IER_ETBEI != 0 && self.thre_intr {
            ISRC_THRE
        } else if self.reg_intr_enable & IER_EDSSI != 0
            && self.reg_modem_status != 0
        {
            // This ignores that MSR is fixed to 0
            ISRC_MDM
        } else {
            ISRC_NONE
        }
    }

    fn update_isr(&mut self) {
        let old = self.reg_intr_status;
        let old_isrc = old & MASK_ISRC;
        let new_isrc = self.next_intr();

        debug_assert!(new_isrc & !MASK_ISRC == 0);
        self.reg_intr_status = (old & !MASK_ISRC) | new_isrc;

        if old_isrc == ISRC_NONE && new_isrc != ISRC_NONE {
            self.intr_pin = true;
        } else if old_isrc != ISRC_NONE && new_isrc == ISRC_NONE {
            self.intr_pin = false;
        }
    }

    fn set_thre(&mut self, state: bool) {
        if state {
            self.reg_line_status |= LSR_THRE | LSR_TEMT;
        } else {
            self.reg_line_status &= !(LSR_THRE | LSR_TEMT);
        }
        if self.thre_intr != state {
            self.thre_intr = state;
            self.update_isr();
        }
    }
    fn update_dr(&mut self) {
        if !self.rx_fifo.is_empty() {
            self.reg_line_status |= LSR_DR;
        } else {
            self.reg_line_status &= !LSR_DR;
        }
    }
    pub(super) fn export(&self) -> migrate::UartV1 {
        migrate::UartV1 {
            intr_enable: self.reg_intr_enable,
            intr_status: self.reg_intr_status,
            line_ctrl: self.reg_line_ctrl,
            line_status: self.reg_line_status,
            modem_ctrl: self.reg_modem_ctrl,
            modem_status: self.reg_modem_status,
            scratch: self.reg_scratch,
            div_low: self.reg_div_low,
            div_high: self.reg_div_high,
            thre_state: self.thre_intr,
        }
    }
}

struct Fifo {
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
    use serde::Serialize;

    #[derive(Serialize)]
    pub struct UartV1 {
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
    }
}

mod bits {
    #![allow(unused)]

    pub const REG_RHR: u8 = 0b000; // RO
    pub const REG_THR: u8 = 0b000; // WO
    pub const REG_IER: u8 = 0b001; // RW
    pub const REG_ISR: u8 = 0b010; // RO
    pub const REG_FCR: u8 = 0b010; // WO
    pub const REG_LCR: u8 = 0b011; // RW
    pub const REG_MCR: u8 = 0b100; // RW
    pub const REG_LSR: u8 = 0b101; // RO
    pub const REG_MSR: u8 = 0b110; // RO
    pub const REG_SPR: u8 = 0b111; // RW
    pub const REG_DLL: u8 = 0b000; // RW when DLAB=1
    pub const REG_DLH: u8 = 0b001; // RW when DLAB=1

    pub const IER_ERBFI: u8 = 1 << 0; // enable received data available intr
    pub const IER_ETBEI: u8 = 1 << 1; // enable xmit holding register empty intr
    pub const IER_ELSI: u8 = 1 << 2; // enable receiver line status intr
    pub const IER_EDSSI: u8 = 1 << 3; // enable modem status intr

    pub const ISRC_NONE: u8 = 0b0001; // no interrupt
    pub const ISRC_RLS: u8 = 0b0110; // receiver line status
    pub const ISRC_DR: u8 = 0b0100; // data ready
    pub const ISRC_TMO: u8 = 0b1100; // character timeout
    pub const ISRC_THRE: u8 = 0b0010; // transmitter holding register empty
    pub const ISRC_MDM: u8 = 0b0000; // modem status

    pub const FCR_ENA: u8 = 1 << 0;
    pub const FCR_RXRST: u8 = 1 << 1;
    pub const FCR_TXRST: u8 = 1 << 2;
    pub const FCR_DMAMD: u8 = 1 << 3;
    pub const FCR_TRGR: u8 = 0b11000000;

    pub const MCR_LOOP: u8 = 1 << 4;

    pub const LSR_DR: u8 = 1 << 0;
    pub const LSR_OE: u8 = 1 << 1;
    pub const LSR_THRE: u8 = 1 << 5;
    pub const LSR_TEMT: u8 = 1 << 6;

    pub const LCR_DLAB: u8 = 0b10000000;

    pub const MASK_PCD: u8 = 0b00001111;
    pub const MASK_MCR: u8 = 0b00011111;
    pub const MASK_IER: u8 = 0b00001111;
    pub const MASK_FCR: u8 = 0b11001111;
    pub const MASK_ISRC: u8 = 0b00001111;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reset_state() {
        let mut uart = Uart::new();
        assert_eq!(uart.reg_read(REG_IER), 0u8);
        assert_eq!(uart.reg_read(REG_ISR), 1u8);
        // TI datasheet notes the state of this register, despite it being WO
        // assert_eq!(uart.reg_fifo_ctrl, 0u8);
        assert_eq!(uart.reg_read(REG_LCR), 0u8);
        assert_eq!(uart.reg_read(REG_MCR), 0u8);
        assert_eq!(uart.reg_read(REG_LSR), 0b01100000u8);
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
        let tval: u8 = 0x20;

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
        let tval: u8 = 0x20;

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
            let _: u8 = uart.reg_read(i);
        }
        for i in 0..=7 {
            uart.reg_write(i, 0xffu8);
        }
    }
    #[test]
    #[should_panic]
    fn invalid_offset() {
        let mut uart = Uart::new();

        uart.reg_read(8);
    }
}
