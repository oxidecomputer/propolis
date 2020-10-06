use std::sync::{Arc, Mutex};

use super::base::Uart;
use crate::pio::PioDev;
use crate::types::*;

pub const COM1_IRQ: u8 = 4;
pub const COM1_PORT: u16 = 0x3f8;

const MAX_OFF: u8 = 7;

pub struct LpcUart {
    irq: u8,
    inner: Mutex<Uart>,
}

impl LpcUart {
    pub fn new(irq: u8) -> Arc<Self> {
        Arc::new(Self { irq, inner: Mutex::new(Uart::new()) })
    }
}

use std::io::Write;

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
        assert!(wo.offset as u8 <= MAX_OFF);
        assert!(!wo.buf.is_empty());

        let mut state = self.inner.lock().unwrap();
        state.reg_write(wo.offset as u8, wo.buf[0]);
        handle_out(&mut *state);
    }
    fn pio_in(&self, _port: u16, ro: &mut ReadOp) {
        assert!(ro.offset as u8 <= MAX_OFF);
        assert!(!ro.buf.is_empty());

        let mut state = self.inner.lock().unwrap();
        ro.buf[0] = state.reg_read(ro.offset as u8);
    }
}
