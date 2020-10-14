use std::sync::{Arc, Mutex, Weak};

use crate::util::self_arc::*;
use crate::vmm::VmmHdl;

pub trait IntrPin {
    fn assert(&mut self);
    fn deassert(&mut self);
    fn is_asserted(&self) -> bool;
    fn pulse(&mut self) {
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

pub struct IsaPIC {
    sa_cell: SelfArcCell<Self>,
    capacity: u8,
    pins: Mutex<Vec<IsaEntry>>,
    hdl: Arc<VmmHdl>,
}

#[derive(Default)]
struct IsaEntry {
    ioapic_irq: u8,
    atpic_irq: Option<u8>,
    atpic_disable: bool,
    level: usize,
}

impl IsaPIC {
    pub fn new(capacity: u8, hdl: Arc<VmmHdl>) -> Arc<Self> {
        assert!(capacity == hdl.ioapic_pin_count().unwrap());
        let mut entries = Vec::with_capacity(capacity as usize);
        for idx in 0..capacity {
            entries
                .push(IsaEntry { ioapic_irq: idx as u8, ..Default::default() });
        }
        let mut this = Arc::new(Self {
            sa_cell: Default::default(),
            capacity,
            pins: Mutex::new(entries),
            hdl,
        });
        SelfArc::self_arc_init(&mut this);
        this
    }

    pub fn set_irq_atpic(&self, pin: u8, irq: Option<u8>) {
        assert!(pin < self.capacity);

        let mut pins = self.pins.lock().unwrap();
        let mut entry = &mut pins[pin as usize];
        match (entry.atpic_irq, irq) {
            (Some(old), Some(new)) if old != new => {
                // XXX: does not handle sharing properly today
                if entry.level != 0 {
                    self.hdl.isa_deassert_irq(old, None).unwrap();
                    self.hdl.isa_assert_irq(new, None).unwrap();
                }
                entry.atpic_irq = Some(new)
            }
            (Some(old), None) => {
                if entry.level != 0 {
                    self.hdl.isa_deassert_irq(old, None).unwrap()
                }
                entry.atpic_irq = None;
            }
            (None, Some(new)) => {
                let mut entry = &mut pins[pin as usize];
                if entry.level != 0 {
                    self.hdl.isa_assert_irq(new, None).unwrap()
                }
                entry.atpic_irq = Some(new)
            }
            _ => {}
        }
    }

    pub fn pin_irq(&self, pin: u8, op: PinOp) {
        assert!(pin < self.capacity);

        let mut pins = self.pins.lock().unwrap();
        let mut entry = &mut pins[pin as usize];
        match (&op, entry.level) {
            (PinOp::Assert, 0) => {
                self.do_pin_irq(entry, op);
                entry.level = 1;
            }
            (PinOp::Deassert, 1) => {
                self.do_pin_irq(entry, op);
                entry.level = 0;
            }
            (PinOp::Pulse, 0) => {
                self.do_pin_irq(entry, op);
            }

            // easy increment/decrement cases
            (PinOp::Assert, _) => {
                entry.level += 1;
            }
            (PinOp::Deassert, _) => {
                assert!(entry.level != 0);
                entry.level -= 1;
            }
            (PinOp::Pulse, _) => {
                // nothing required!
            }
        }
    }

    fn do_pin_irq(&self, entry: &IsaEntry, op: PinOp) {
        if entry.atpic_irq.is_none() || entry.atpic_disable {
            let ioapic_irq = entry.ioapic_irq;
            match op {
                PinOp::Assert => {
                    self.hdl.ioapic_assert_irq(ioapic_irq).unwrap();
                }
                PinOp::Deassert => {
                    self.hdl.ioapic_deassert_irq(ioapic_irq).unwrap();
                }
                PinOp::Pulse => {
                    self.hdl.ioapic_pulse_irq(ioapic_irq).unwrap();
                }
            }
        } else {
            let ioapic_irq = entry.ioapic_irq;
            let atpic_irq = entry.atpic_irq.clone().unwrap();
            match op {
                PinOp::Assert => {
                    self.hdl
                        .isa_assert_irq(atpic_irq, Some(ioapic_irq))
                        .unwrap();
                }
                PinOp::Deassert => {
                    self.hdl
                        .isa_deassert_irq(atpic_irq, Some(ioapic_irq))
                        .unwrap();
                }
                PinOp::Pulse => {
                    self.hdl
                        .isa_pulse_irq(atpic_irq, Some(ioapic_irq))
                        .unwrap();
                }
            }
        }
    }

    pub fn pin_handle(&self, pin: u8) -> Option<IsaPin> {
        assert!(pin < self.capacity);
        Some(IsaPin { asserted: false, pin, pic: self.self_weak() })
    }
}

impl SelfArc for IsaPIC {
    fn self_arc_cell(&self) -> &SelfArcCell<Self> {
        &self.sa_cell
    }
}

pub struct IsaPin {
    asserted: bool,
    pin: u8,
    pic: Weak<IsaPIC>,
}
impl IsaPin {
    pub fn get_pin(&self) -> u8 {
        self.pin
    }
    pub fn set(&mut self, assert: bool) {
        if assert {
            self.assert()
        } else {
            self.deassert()
        }
    }
}

impl IntrPin for IsaPin {
    fn assert(&mut self) {
        if !self.asserted {
            self.asserted = true;
            if let Some(pic) = Weak::upgrade(&self.pic) {
                pic.pin_irq(self.pin, PinOp::Assert);
            }
        }
    }
    fn deassert(&mut self) {
        if self.asserted {
            self.asserted = false;
            if let Some(pic) = Weak::upgrade(&self.pic) {
                pic.pin_irq(self.pin, PinOp::Deassert);
            }
        }
    }
    fn pulse(&mut self) {
        if !self.asserted {
            if let Some(pic) = Weak::upgrade(&self.pic) {
                pic.pin_irq(self.pin, PinOp::Pulse);
            }
        }
    }
    fn is_asserted(&self) -> bool {
        self.asserted
    }
}
