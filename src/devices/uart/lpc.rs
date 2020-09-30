use super::base::Uart;
use crate::pio::PioDev;
use std::sync::Mutex;

pub const COM1_IRQ: u8 = 4;
pub const COM1_PORT: u16 = 0x3f8;

const MAX_OFF: u16 = 7;

pub struct LpcUart {
    irq: u8,
    inner: Mutex<Uart>,
}

impl LpcUart {
    pub fn new(irq: u8) -> Self {
        Self {
            irq,
            inner: Mutex::new(Uart::new()),
        }
    }
}

use std::io::Write;

fn handle_out(uart: &mut Uart) {
    if uart.is_readable() {
        let stdout = std::io::stdout();
        let mut hdl = stdout.lock();
        let buf = uart.data_read().unwrap();
        hdl.write(&[buf]).unwrap();
    }
}

impl PioDev for LpcUart {
    fn pio_out(&self, _port: u16, off: u16, data: &[u8]) {
        assert!(off <= MAX_OFF);
        assert!(data.len() != 0);

        let mut state = self.inner.lock().unwrap();
        state.reg_write(off as u8, data[0]);
        handle_out(&mut *state);
    }
    fn pio_in(&self, _port: u16, off: u16, data: &mut [u8]) {
        assert!(off <= MAX_OFF);
        assert!(data.len() != 0);

        let mut state = self.inner.lock().unwrap();
        data[0] = state.reg_read(off as u8);
    }
}
