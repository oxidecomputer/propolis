#![allow(clippy::mutex_atomic)]

use std::sync::{Arc, Mutex, Weak};

use crate::vmm::VmmHdl;

const PIN_COUNT: u8 = 16;

pub trait IntrPin: Send + Sync + 'static {
    fn assert(&self);
    fn deassert(&self);
    fn is_asserted(&self) -> bool;
    fn pulse(&self) {
        if !self.is_asserted() {
            self.assert();
            self.deassert();
        }
    }
}

/// Describes the operation to take with an interrupt pin.
pub enum PinOp {
    /// Asserts the interrupt.
    Assert,
    /// Deasserts the interrupt.
    Deassert,
    /// Asserts and then deasserts the interrupt.
    Pulse,
}

pub struct LegacyPIC {
    inner: Mutex<Inner>,
    hdl: Arc<VmmHdl>,
}

struct Inner {
    pins: [Entry; PIN_COUNT as usize],
}

#[derive(Default, Copy, Clone)]
struct Entry {
    level: usize,
}
impl Entry {
    fn process_op(&mut self, op: &PinOp) -> bool {
        match op {
            PinOp::Assert => {
                self.level += 1;
                // Notify if going 0->1
                self.level == 1
            }
            PinOp::Deassert => {
                assert!(self.level != 0);
                self.level -= 1;
                // Notify if going 1->0
                self.level == 0
            }
            PinOp::Pulse => {
                // Notify if going 0->1->0
                self.level == 0
            }
        }
    }
}

impl LegacyPIC {
    /// Creates a new virtual PIC.
    pub fn new(hdl: Arc<VmmHdl>) -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(Inner {
                pins: [Entry::default(); PIN_COUNT as usize],
            }),
            hdl,
        })
    }

    pub fn pin_handle(self: &Arc<Self>, irq: u8) -> Option<LegacyPin> {
        if irq >= PIN_COUNT && irq == 2 {
            return None;
        }
        Some(LegacyPin::new(irq, Arc::downgrade(self)))
    }

    fn do_irq(&self, op: PinOp, irq: u8) {
        assert!(irq < PIN_COUNT);

        let mut inner = self.inner.lock().unwrap();
        if inner.pins[irq as usize].process_op(&op) {
            match op {
                PinOp::Assert => {
                    self.hdl.isa_assert_irq(irq, Some(irq)).unwrap();
                }
                PinOp::Deassert => {
                    self.hdl.isa_deassert_irq(irq, Some(irq)).unwrap();
                }
                PinOp::Pulse => {
                    self.hdl.isa_pulse_irq(irq, Some(irq)).unwrap();
                }
            }
        }
    }
}

pub struct LegacyPin {
    irq: u8,
    asserted: Mutex<bool>,
    pic: Weak<LegacyPIC>,
}
impl LegacyPin {
    fn new(irq: u8, pic: Weak<LegacyPIC>) -> Self {
        Self { irq, asserted: Mutex::new(false), pic }
    }
    pub fn set_state(&self, is_asserted: bool) {
        if is_asserted {
            self.assert();
        } else {
            self.deassert();
        }
    }
}
impl IntrPin for LegacyPin {
    fn assert(&self) {
        let mut asserted = self.asserted.lock().unwrap();
        if !*asserted {
            *asserted = true;
            if let Some(pic) = Weak::upgrade(&self.pic) {
                pic.do_irq(PinOp::Assert, self.irq);
            }
        }
    }
    fn deassert(&self) {
        let mut asserted = self.asserted.lock().unwrap();
        if *asserted {
            *asserted = false;
            if let Some(pic) = Weak::upgrade(&self.pic) {
                pic.do_irq(PinOp::Deassert, self.irq);
            }
        }
    }
    fn pulse(&self) {
        let asserted = self.asserted.lock().unwrap();
        if !*asserted {
            if let Some(pic) = Weak::upgrade(&self.pic) {
                pic.do_irq(PinOp::Pulse, self.irq);
            }
        }
    }
    fn is_asserted(&self) -> bool {
        let asserted = self.asserted.lock().unwrap();
        *asserted
    }
}
