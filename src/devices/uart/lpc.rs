use std::sync::Mutex;
use super::base::Uart;
use crate::inout::InoutDev;

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
            inner: Mutex::new(Uart::new())
        }
    }
}

impl InoutDev for LpcUart {
    fn pio_out(&self, off: u16, data: &[u8]) {
        assert!(off <= MAX_OFF);
        assert!(data.len() != 0);

        let mut state = self.inner.lock().unwrap();
        state.reg_write(off as u8, data[0]);

    }
    fn pio_in(&self, off: u16, data: &mut [u8]) {
        assert!(off <= MAX_OFF);
        assert!(data.len() != 0);

        let mut state = self.inner.lock().unwrap();
        data[0] = state.reg_read(off as u8);
    }
}
