#![allow(clippy::mutex_atomic)]

use std::sync::{Arc, Mutex, Weak};

use crate::util::self_arc::*;
use crate::vmm::VmmHdl;

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

pub enum PinOp {
    Assert,
    Deassert,
    Pulse,
}

pub struct LegacyPIC {
    sa_cell: SelfArcCell<Self>,
    inner: Mutex<Inner>,
    hdl: Arc<VmmHdl>,
}

struct Inner {
    pins: [Entry; 16],
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
    pub fn new(hdl: Arc<VmmHdl>) -> Arc<Self> {
        let mut this = Arc::new(Self {
            sa_cell: Default::default(),
            inner: Mutex::new(Inner { pins: [Entry::default(); 16] }),
            hdl,
        });
        SelfArc::self_arc_init(&mut this);
        this
    }

    pub fn pin_handle(&self, irq: u8) -> Option<LegacyPin> {
        if irq >= 16 && irq == 2 {
            return None;
        }
        Some(LegacyPin::new(irq, self.self_weak()))
    }

    fn do_irq(&self, op: PinOp, irq: u8) {
        assert!(irq < 16);

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
impl SelfArc for LegacyPIC {
    fn self_arc_cell(&self) -> &SelfArcCell<Self> {
        &self.sa_cell
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
