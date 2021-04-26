use std::sync::{Arc, Mutex, Weak};

use super::base::Uart;
use crate::chardev::*;
use crate::common::*;
use crate::dispatch::DispCtx;
use crate::intr_pins::{IntrPin, LegacyPin};
use crate::pio::{PioBus, PioDev};

pub const REGISTER_LEN: usize = 8;

struct UartState {
    uart: Uart,
    irq_pin: LegacyPin,
    auto_discard: bool,
}
struct Notifiers {
    notify_readable: Option<Notifier<DispCtx>>,
    notify_writable: Option<Notifier<DispCtx>>,
}

impl UartState {
    fn sync_intr_pin(&self) {
        if self.uart.intr_state() {
            self.irq_pin.assert()
        } else {
            self.irq_pin.deassert()
        }
    }
}

pub struct LpcUart {
    state: Mutex<UartState>,
    notifiers: Mutex<Notifiers>,
}

impl LpcUart {
    pub fn new(irq_pin: LegacyPin) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(UartState {
                uart: Uart::new(),
                irq_pin,
                auto_discard: true,
            }),
            notifiers: Mutex::new(Notifiers {
                notify_readable: None,
                notify_writable: None,
            }),
        })
    }
    pub fn attach(self: &Arc<Self>, bus: &PioBus, port: u16) {
        bus.register(
            port,
            REGISTER_LEN as u16,
            Arc::downgrade(self) as Weak<dyn PioDev>,
            0,
        )
        .unwrap();
    }
}

impl Sink<DispCtx> for LpcUart {
    fn sink_write(&self, data: u8) -> bool {
        let mut state = self.state.lock().unwrap();
        let res = state.uart.data_write(data);
        state.sync_intr_pin();
        res
    }
    fn sink_set_notifier(&self, f: Notifier<DispCtx>) {
        let mut notifiers = self.notifiers.lock().unwrap();
        notifiers.notify_writable = Some(f);
    }
}
impl Source<DispCtx> for LpcUart {
    fn source_read(&self) -> Option<u8> {
        let mut state = self.state.lock().unwrap();
        let res = state.uart.data_read();
        state.sync_intr_pin();
        res
    }
    fn source_discard(&self, count: usize) -> usize {
        let mut state = self.state.lock().unwrap();
        let mut discarded = 0;
        while discarded < count {
            if let Some(_val) = state.uart.data_read() {
                discarded += 1;
            } else {
                break;
            }
        }
        state.sync_intr_pin();
        discarded
    }
    fn source_set_notifier(&self, f: Notifier<DispCtx>) {
        let mut notifiers = self.notifiers.lock().unwrap();
        notifiers.notify_readable = Some(f);
    }
    fn source_set_autodiscard(&self, active: bool) {
        let mut state = self.state.lock().unwrap();
        state.auto_discard = active;
    }
}

impl PioDev for LpcUart {
    fn pio_rw(&self, _port: u16, _ident: usize, rwo: RWOp, ctx: &DispCtx) {
        assert!(rwo.offset() < REGISTER_LEN);
        assert!(rwo.len() != 0);
        let mut state = self.state.lock().unwrap();
        let readable_before = state.uart.is_readable();
        let writable_before = state.uart.is_writable();

        match rwo {
            RWOp::Read(ro) => {
                ro.write_u8(state.uart.reg_read(ro.offset() as u8));
            }
            RWOp::Write(wo) => {
                state.uart.reg_write(wo.offset() as u8, wo.read_u8());
            }
        }
        if state.auto_discard {
            while let Some(_val) = state.uart.data_read() {}
        }

        state.sync_intr_pin();

        let read_notify = !readable_before && state.uart.is_readable();
        let write_notify = !writable_before && state.uart.is_writable();

        // The uart state lock cannot be held while dispatching notifications since those callbacks
        // could immediately attempt to read/write the pending data.
        drop(state);
        if read_notify || write_notify {
            let notifiers = self.notifiers.lock().unwrap();
            if read_notify {
                if let Some(cb) = notifiers.notify_readable.as_ref() {
                    cb(ctx);
                }
            }
            if write_notify {
                if let Some(cb) = notifiers.notify_writable.as_ref() {
                    cb(ctx);
                }
            }
        }
    }
}
impl Entity for LpcUart {}
