// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

#![allow(clippy::mutex_atomic)]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock, Weak};

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
    fn set_state(&self, is_asserted: bool) {
        if is_asserted {
            self.assert();
        } else {
            self.deassert();
        }
    }

    /// Set the state of this interrupt pin *without* treating the state change
    /// as an edge in the interrupt line. Used when importing a guest.
    ///
    /// This method differs from [`IntrPin::set_state`], as it updates the
    /// internal accounting of the pin's state without notifying consumers of
    /// the interrupt pin of a rising/falling edge. This is because it's called
    /// during an import of a guest from a different VMM, which has *already*
    /// observed the edge event; this VMM is just updating its internal state to
    /// match the imported state.
    ///
    /// For example, when importing a guest which currently has a [`LegacyPIC`]
    /// pin asserted, `import_state` will not call the `ioctl` that asserts that
    /// pin in the kernel VMM, while [`IntrPin::set_state`] would. This is
    /// important, as the kernel-emulated interrupt state is imported separately
    /// from the userspace state, so calling [`IntrPin::set_state`] would result
    /// in inconsistent state between the kernel and userspace.
    fn import_state(&self, is_asserted: bool);
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

    fn import_irq(&self, op: PinOp, irq: u8) {
        assert!(irq < PIN_COUNT);

        let mut inner = self.inner.lock().unwrap();

        // Update our tracked pin level count, but *don't* actually perform the
        // ioctl to assert the bhyve interrupt, since the kernelspace pin states
        // are imported separately.
        inner.pins[irq as usize].process_op(&op);
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
    fn import_state(&self, is_asserted: bool) {
        let mut asserted = self.asserted.lock().unwrap();
        if *asserted != is_asserted {
            *asserted = is_asserted;
            if let Some(pic) = Weak::upgrade(&self.pic) {
                let op =
                    if is_asserted { PinOp::Assert } else { PinOp::Deassert };
                pic.import_irq(op, self.irq);
            }
        }
    }
}

/// Interrupt pin which calls a provided function on rising and falling edges.
///
/// The consumer-provided function is called when the pin undergoes a state
/// transition (`low->high` or `high->low`) with its argument corresponding to the
/// pin level after the transition (true = high).
///
/// That function call is made under the protection of a mutex, excluding all
/// other operations on the pin until it returns.
pub struct FuncPin(Mutex<FPInner>);
impl FuncPin {
    pub fn new(func: Box<dyn Fn(bool) + Send + 'static>) -> Self {
        Self(Mutex::new(FPInner { level: false, func }))
    }
}
impl IntrPin for FuncPin {
    fn assert(&self) {
        let mut inner = self.0.lock().unwrap();
        if !inner.level {
            inner.level = true;
            (inner.func)(inner.level);
        }
    }
    fn deassert(&self) {
        let mut inner = self.0.lock().unwrap();
        if inner.level {
            inner.level = false;
            (inner.func)(inner.level);
        }
    }
    fn is_asserted(&self) -> bool {
        let inner = self.0.lock().unwrap();
        inner.level
    }
    fn import_state(&self, is_asserted: bool) {
        let mut inner = self.0.lock().unwrap();
        // Set the state to the imported state without calling the function ---
        // presumably, whatever the function does already happened prior to the
        // import.
        inner.level = is_asserted;
    }
}
struct FPInner {
    level: bool,
    func: Box<dyn Fn(bool) + Send + 'static>,
}

pub struct OrGate {
    inputs: RwLock<Vec<AtomicBool>>,
    output: Arc<dyn IntrPin>,
}
impl OrGate {
    pub fn new(pin: Arc<dyn IntrPin>) -> Arc<Self> {
        Arc::new(Self { output: pin, inputs: RwLock::new(vec![]) })
    }

    pub fn new_input(self: &Arc<Self>) -> Arc<dyn IntrPin> {
        let mut inputs = self.inputs.write().unwrap();
        let pin = Arc::new(OrGatePin { idx: inputs.len(), gate: self.clone() });
        inputs.push(AtomicBool::new(false));
        pin
    }

    fn set_input_state(&self, input: usize, is_asserted: bool) {
        let was_asserted = {
            let inputs = self.inputs.read().unwrap();
            assert!(input < inputs.len());

            inputs[input].swap(is_asserted, Ordering::Relaxed)
        };
        if is_asserted != was_asserted {
            self.set_output_state();
        }
    }

    fn pulse_input(&self, input: usize) {
        let should_pulse = {
            let inputs = self.inputs.read().unwrap();
            assert!(input < inputs.len());

            let pulsed = inputs[input].compare_exchange(
                false,
                true,
                Ordering::Relaxed,
                Ordering::Relaxed,
            );
            match pulsed {
                Ok(_) => {
                    inputs[input].store(false, Ordering::Relaxed);
                    true
                }
                Err(_) => false,
            }
        };
        if should_pulse {
            self.pulse_output();
        }
    }

    fn is_input_asserted(&self, input: usize) -> bool {
        let inputs = self.inputs.read().unwrap();
        assert!(input < inputs.len());

        inputs[input].load(Ordering::Relaxed)
    }

    fn import_input_state(&self, input: usize, is_asserted: bool) {
        let was_asserted = {
            let inputs = self.inputs.read().unwrap();
            assert!(input < inputs.len());

            inputs[input].swap(is_asserted, Ordering::Relaxed)
        };
        if is_asserted != was_asserted {
            self.import_output_state();
        }
    }

    #[allow(clippy::readonly_write_lock)]
    fn set_output_state(&self) {
        let inputs = self.inputs.write().unwrap();
        let should_assert = OrGate::is_asserted(&inputs);
        if should_assert != self.output.is_asserted() {
            self.output.set_state(should_assert);
        }
    }

    #[allow(clippy::readonly_write_lock)]
    fn pulse_output(&self) {
        let inputs = self.inputs.write().unwrap();
        if !OrGate::is_asserted(&inputs) {
            self.output.pulse();
        }
    }

    #[allow(clippy::readonly_write_lock)]
    fn import_output_state(&self) {
        let inputs = self.inputs.write().unwrap();
        let should_assert = OrGate::is_asserted(&inputs);
        if should_assert != self.output.is_asserted() {
            self.output.import_state(should_assert);
        }
    }

    fn is_asserted(inputs: &[AtomicBool]) -> bool {
        inputs.iter().any(|b| b.load(Ordering::Relaxed))
    }
}

pub struct OrGatePin {
    idx: usize,
    gate: Arc<OrGate>,
}
impl IntrPin for OrGatePin {
    fn assert(&self) {
        self.gate.set_input_state(self.idx, true);
    }
    fn deassert(&self) {
        self.gate.set_input_state(self.idx, false);
    }
    fn pulse(&self) {
        self.gate.pulse_input(self.idx);
    }
    fn is_asserted(&self) -> bool {
        self.gate.is_input_asserted(self.idx)
    }
    fn import_state(&self, is_asserted: bool) {
        self.gate.import_input_state(self.idx, is_asserted)
    }
}
impl Drop for OrGatePin {
    fn drop(&mut self) {
        self.deassert();
    }
}

pub struct NoOpPin {}

impl IntrPin for NoOpPin {
    fn assert(&self) {}
    fn deassert(&self) {}
    fn pulse(&self) {}
    fn is_asserted(&self) -> bool {
        false
    }
    fn import_state(&self, _: bool) {}
}

#[cfg(test)]
pub mod test {
    use super::*;

    use std::hint;
    use std::sync::Barrier;
    use std::thread;

    struct TestPinState {
        is_asserted: bool,
        rise_count: usize,
        fall_count: usize,
        pulse_count: usize,
    }
    impl TestPinState {
        fn assert(&mut self) {
            if !self.is_asserted {
                self.is_asserted = true;
                self.rise_count += 1;
            }
        }
        fn deassert(&mut self) {
            if self.is_asserted {
                self.is_asserted = false;
                self.fall_count += 1;
            }
        }
        fn pulse(&mut self) {
            if !self.is_asserted {
                self.rise_count += 1;
                self.fall_count += 1;
                self.pulse_count += 1;
            }
        }
        fn import_state(&mut self, is_asserted: bool) {
            self.is_asserted = is_asserted;
        }
    }

    pub struct TestPin {
        state: Mutex<TestPinState>,
    }
    impl TestPin {
        pub fn new() -> Self {
            Self {
                state: Mutex::new(TestPinState {
                    is_asserted: false,
                    rise_count: 0,
                    fall_count: 0,
                    pulse_count: 0,
                }),
            }
        }
        pub fn rise_count(&self) -> usize {
            self.state.lock().unwrap().rise_count
        }
        pub fn fall_count(&self) -> usize {
            self.state.lock().unwrap().fall_count
        }
        pub fn pulse_count(&self) -> usize {
            self.state.lock().unwrap().pulse_count
        }
    }
    impl IntrPin for TestPin {
        fn assert(&self) {
            self.state.lock().unwrap().assert();
        }
        fn deassert(&self) {
            self.state.lock().unwrap().deassert();
        }
        fn pulse(&self) {
            self.state.lock().unwrap().pulse();
        }
        fn is_asserted(&self) -> bool {
            self.state.lock().unwrap().is_asserted
        }
        fn import_state(&self, is_asserted: bool) {
            self.state.lock().unwrap().import_state(is_asserted);
        }
    }

    type ConcurrentPinSignal =
        Arc<dyn Fn(&Arc<dyn IntrPin>, usize) + Send + Sync>;
    struct ConcurrentPinDriver {
        gate: Arc<OrGate>,
        output: Arc<TestPin>,
        start_asserted: bool,

        pins: Vec<Arc<dyn IntrPin>>,
        signals: Vec<ConcurrentPinSignal>,
    }
    impl ConcurrentPinDriver {
        fn new() -> Self {
            Self::new_inner(false)
        }

        fn new_asserted() -> Self {
            Self::new_inner(true)
        }

        fn new_inner(start_asserted: bool) -> Self {
            let output = Arc::new(TestPin::new());
            Self {
                output: output.clone(),
                gate: OrGate::new(output.clone()),
                start_asserted,
                pins: vec![],
                signals: vec![],
            }
        }

        fn add_signal(&mut self, f: ConcurrentPinSignal) {
            let pin = self.gate.new_input();
            self.pins.push(pin);
            self.signals.push(f);
        }

        fn run(&self, ticks: usize) {
            let start = Arc::new(Barrier::new(self.pins.len() + 1));
            let done = Arc::new(Barrier::new(self.pins.len() + 1));
            let go = Arc::new(AtomicBool::new(false));

            let threads = self
                .pins
                .iter()
                .enumerate()
                .map(|(pin_idx, pin)| {
                    if self.start_asserted {
                        pin.import_state(true);
                    }

                    let start = start.clone();
                    let done = done.clone();
                    let go = go.clone();
                    let pin = pin.clone();
                    let f = self.signals[pin_idx].clone();

                    thread::spawn(move || {
                        for i in 0..ticks {
                            start.wait();
                            while !go.load(Ordering::Acquire) {
                                hint::spin_loop();
                            }

                            f(&pin, i);

                            done.wait();
                        }
                    })
                })
                .collect::<Vec<_>>();

            for _ in 0..ticks {
                go.store(false, Ordering::Release);
                start.wait();
                go.store(true, Ordering::Release);
                done.wait();
            }

            threads.into_iter().for_each(|t| t.join().unwrap());
        }
    }

    #[test]
    fn test_or_gate() {
        let output_pin = Arc::new(TestPin::new());
        let or_gate = OrGate::new(output_pin.clone());

        let pin1 = or_gate.new_input();
        let pin2 = or_gate.new_input();
        let pin3 = or_gate.new_input();

        // Assert pin1. Expect gate output to go high.
        pin1.assert();
        assert!(pin1.is_asserted());
        assert!(output_pin.is_asserted());
        assert_eq!(output_pin.rise_count(), 1);
        assert_eq!(output_pin.fall_count(), 0);

        // Assert pin2. Expect gate output to remain high and not re-assert.
        pin2.assert();
        assert!(pin2.is_asserted());
        assert!(output_pin.is_asserted());
        assert_eq!(output_pin.rise_count(), 1);
        assert_eq!(output_pin.fall_count(), 0);

        // Deassert pin1. Expect gate output to remain high.
        pin1.deassert();
        assert!(!pin1.is_asserted());
        assert!(output_pin.is_asserted());
        assert_eq!(output_pin.rise_count(), 1);
        assert_eq!(output_pin.fall_count(), 0);

        // Deassert pin2. Expect gate output to go low.
        pin2.deassert();
        assert!(!pin2.is_asserted());
        assert!(!output_pin.is_asserted());
        assert_eq!(output_pin.rise_count(), 1);
        assert_eq!(output_pin.fall_count(), 1);

        // Deassert deasserted pin3 while gate output is low. Expect no-op.
        pin3.deassert();
        assert!(!pin3.is_asserted());
        assert!(!output_pin.is_asserted());
        assert_eq!(output_pin.rise_count(), 1);
        assert_eq!(output_pin.fall_count(), 1);
    }

    #[test]
    fn test_or_gate_pulse() {
        let output_pin = Arc::new(TestPin::new());
        let or_gate = OrGate::new(output_pin.clone());

        let pin1 = or_gate.new_input();
        let pin2 = or_gate.new_input();

        // Pulse while gate output is low. Expect output pulse.
        pin1.pulse();
        assert!(!output_pin.is_asserted());
        assert_eq!(output_pin.rise_count(), 1);
        assert_eq!(output_pin.fall_count(), 1);
        assert_eq!(output_pin.pulse_count(), 1);

        // Pulse while gate output is high. Expect no-op.
        pin1.assert();
        pin2.pulse();
        assert!(output_pin.is_asserted());
        assert_eq!(output_pin.rise_count(), 2);
        assert_eq!(output_pin.fall_count(), 1);
        assert_eq!(output_pin.pulse_count(), 1);

        // Pulse pin that is already high. Expect no-op.
        assert!(pin1.is_asserted());
        pin1.pulse();
        assert!(output_pin.is_asserted());
        assert_eq!(output_pin.rise_count(), 2);
        assert_eq!(output_pin.fall_count(), 1);
        assert_eq!(output_pin.pulse_count(), 1);
    }

    #[test]
    fn test_or_gate_drop_input() {
        let output_pin = Arc::new(TestPin::new());
        let or_gate = OrGate::new(output_pin.clone());

        let pin1 = or_gate.new_input();
        let pin2 = or_gate.new_input();
        let pin3 = or_gate.new_input();

        // Assert two of the inputs.
        pin1.assert();
        pin2.assert();
        assert!(output_pin.is_asserted());

        // Drop one asserted input. Expect gate output to remain high.
        drop(pin1);
        assert!(output_pin.is_asserted());

        // Drop the other asserted input. Expect gate output to go low.
        drop(pin2);
        assert!(!output_pin.is_asserted());

        // Drop last input. Expect gate output to remain unchanged.
        drop(pin3);
        assert!(!output_pin.is_asserted());
    }

    #[test]
    fn test_or_gate_import_state() {
        let output_pin = Arc::new(TestPin::new());
        let or_gate = OrGate::new(output_pin.clone());

        let pin1 = or_gate.new_input();
        let pin2 = or_gate.new_input();
        let pin3 = or_gate.new_input();

        pin1.import_state(false);
        assert!(!output_pin.is_asserted());
        assert_eq!(output_pin.rise_count(), 0);

        pin2.import_state(true);
        assert!(output_pin.is_asserted());
        assert_eq!(output_pin.rise_count(), 0);

        pin3.import_state(false);
        assert!(output_pin.is_asserted());
        assert_eq!(output_pin.rise_count(), 0);
    }

    // Assert pins concurrently. Expect output to go high just once.
    #[test]
    fn test_or_gate_concurrent_assert() {
        let mut driver = ConcurrentPinDriver::new();
        (0..10).for_each(|_| {
            driver.add_signal(Arc::new(move |pin, _| {
                pin.assert();
            }));
        });
        driver.run(100);

        assert!(driver.output.is_asserted());
        assert_eq!(driver.output.rise_count(), 1);
        assert_eq!(driver.output.fall_count(), 0);
    }

    // Deassert pins concurrently. Expect output to go low just once.
    #[test]
    fn test_or_gate_concurrent_deassert() {
        let mut driver = ConcurrentPinDriver::new_asserted();
        (0..10).for_each(|_| {
            driver.add_signal(Arc::new(move |pin, _| {
                pin.deassert();
            }));
        });
        driver.run(100);

        assert!(!driver.output.is_asserted());
        assert_eq!(driver.output.rise_count(), 0);
        assert_eq!(driver.output.fall_count(), 1);
    }

    // Pulse pins concurrently. Expect all pulses to be handled.
    #[test]
    fn test_or_gate_concurrent_pulse() {
        let mut driver = ConcurrentPinDriver::new();
        (0..10).for_each(|_| {
            driver.add_signal(Arc::new(move |pin, _| {
                pin.pulse();
            }));
        });
        driver.run(100);

        assert!(!driver.output.is_asserted());
        assert_eq!(driver.output.pulse_count(), 1000);
    }

    // Assert and deassert two pins with square waves in lockstep.
    //
    // Expect output to match square wave. The thread that wins will drive
    // the output, while the other thread action will be a no-op.
    //
    //         ┌─┐ ┌─┐ ┌─┐ ┌─┐
    //   pin1 ─┘ └─┘ └─┘ └─┘ └─
    //         ┌─┐ ┌─┐ ┌─┐ ┌─┐
    //   pin2 ─┘ └─┘ └─┘ └─┘ └─
    //         ┌─┐ ┌─┐ ┌─┐ ┌─┐
    // output ─┘ └─┘ └─┘ └─┘ └─
    #[test]
    fn test_or_gate_concurrent_square_wave() {
        let mut driver = ConcurrentPinDriver::new();
        driver.add_signal(Arc::new(move |pin, i| {
            if i % 2 == 0 {
                pin.assert();
            } else {
                pin.deassert();
            }
        }));
        driver.add_signal(Arc::new(move |pin, i| {
            if i % 2 == 0 {
                pin.assert();
            } else {
                pin.deassert();
            }
        }));
        driver.run(100);

        assert!(!driver.output.is_asserted());
        assert_eq!(driver.output.rise_count(), 50);
        assert_eq!(driver.output.fall_count(), 50);
    }

    // Assert and deassert a pin with a square wave while pulsing a second
    // pin.
    //
    // There are four possible outcomes:
    //   1. pin1 asserts after pin2 pulses => output pulses.
    //   2. pin1 deasserts after pin2 pulses => pulsed while high, no-op.
    //   3. pin1 asserts before pin2 pulses => pulsed while high, no-op.
    //   4. pin1 deasserts before pin2 pulses => output pulses.
    //
    //         ┌─┐ ┌─┐
    //   pin1 ─┘ └─┘ └──
    //         ▲ ▲ ▲ ▲
    //   pin2 ─┴─┴─┴─┴──
    //         ▲┌┐ ┌─┐▲
    // output ─┴┘└─┘ └┴─
    //
    // In theory we would expect that about half the pulses to cause the output
    // to pulse as well, but in practice the threads will race to lock the
    // gate, so actual pulse count is non-deterministic.
    //
    // Expect to at least not have any spurious edges or pulses.
    #[test]
    fn test_or_gate_concurrent_pulse_with_square_wave() {
        let mut driver = ConcurrentPinDriver::new();
        driver.add_signal(Arc::new(move |pin, i| {
            if i % 2 == 0 {
                pin.assert();
            } else {
                pin.deassert();
            }
        }));
        driver.add_signal(Arc::new(move |pin, _| {
            pin.pulse();
        }));
        driver.run(100);

        assert!(!driver.output.is_asserted());
        assert_eq!(driver.output.rise_count(), driver.output.fall_count());
        assert!(driver.output.pulse_count() <= 100);
    }
}
