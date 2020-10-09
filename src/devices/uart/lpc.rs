use std::io::Write;
use std::sync::{Arc, Mutex};

use super::base::Uart;
use crate::intr_pins::IsaPin;
use crate::pio::PioDev;
use crate::types::*;

pub const REGISTER_LEN: usize = 8;

pub struct LpcUart(Mutex<Inner>);
struct Inner {
    uart: Uart,
    irq_pin: IsaPin,
}

impl LpcUart {
    pub fn new(irq_pin: IsaPin) -> Arc<Self> {
        Arc::new(Self(Mutex::new(Inner { uart: Uart::new(), irq_pin })))
    }
}

fn handle_out(uart: &mut Uart) {
    if uart.is_readable() {
        let stdout = std::io::stdout();
        let mut hdl = stdout.lock();
        let buf = uart.data_read().unwrap();
        hdl.write_all(&[buf]).unwrap();
    }
}

impl PioDev for LpcUart {
    fn pio_out(&self, _port: u16, wo: &WriteOp) {
        assert!(wo.offset < REGISTER_LEN);
        assert!(!wo.buf.is_empty());

        let mut this = self.0.lock().unwrap();
        this.uart.reg_write(wo.offset as u8, wo.buf[0]);
        handle_out(&mut this.uart);
    }
    fn pio_in(&self, _port: u16, ro: &mut ReadOp) {
        assert!(ro.offset < REGISTER_LEN);
        assert!(!ro.buf.is_empty());

        let mut this = self.0.lock().unwrap();
        ro.buf[0] = this.uart.reg_read(ro.offset as u8);
        let intr_active = this.uart.intr_state();
        this.irq_pin.set(intr_active);
    }
}
