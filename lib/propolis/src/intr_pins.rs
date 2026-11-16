// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

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

/// Aggregates multiple input pins into a single `output` [IntrPin] using an OR
/// operation. Inputs are also [IntrPin] themselves. Use [OrGate::new_input] to
/// retrieve a new input.
///
/// The gate is considered stateless for migration purposes. It's expected that
/// the components connected to the inputs manage and reconstruct their own
/// state after a migration.
///
/// The current implementation uses a single [std::sync::Mutex], and is
/// overly-protective of concurrent operations. It would be possible to allow
/// inputs to change independently, but we would need to be careful to also
/// synchronize updates to the output pin.
pub struct OrGate {
    inner: Mutex<OrGateInner>,
}
impl OrGate {
    pub fn new(pin: Arc<dyn IntrPin>) -> Arc<Self> {
        Arc::new(Self { inner: Mutex::new(OrGateInner::new(pin)) })
    }

    pub fn new_input(self: &Arc<Self>) -> Arc<dyn IntrPin> {
        let mut inner = self.inner.lock().unwrap();

        let idx = inner.idx;
        assert!(idx < 64, "max number of inputs reached");

        inner.idx += 1;
        Arc::new(OrGatePin { mask: 1 << idx, gate: self.clone() })
    }

    fn set_input_state(&self, mask: u64, is_asserted: bool) {
        let mut inner = self.inner.lock().unwrap();

        if is_asserted {
            inner.inputs.assert(mask)
        } else {
            inner.inputs.deassert(mask)
        };

        // Only modify the output if the gate state actually changed to avoid
        // spurious edges.
        let should_assert = inner.inputs.is_asserted();
        if should_assert != inner.output.is_asserted() {
            inner.output.set_state(should_assert);
        }
    }

    fn pulse_input(&self) {
        let inner = self.inner.lock().unwrap();
        if !inner.inputs.is_asserted() {
            inner.output.pulse();
        }
    }

    fn is_input_asserted(&self, mask: u64) -> bool {
        let inner = self.inner.lock().unwrap();
        inner.inputs.is_input_asserted(mask)
    }

    fn import_input_state(&self, mask: u64, is_asserted: bool) {
        let mut inner = self.inner.lock().unwrap();

        if is_asserted {
            inner.inputs.assert(mask)
        } else {
            inner.inputs.deassert(mask)
        };

        let should_assert = inner.inputs.is_asserted();
        if should_assert != inner.output.is_asserted() {
            inner.output.import_state(should_assert);
        }
    }
}

struct OrGateInner {
    idx: u8,
    inputs: OrGateInputs,
    output: Arc<dyn IntrPin>,
}
impl OrGateInner {
    fn new(pin: Arc<dyn IntrPin>) -> Self {
        Self { idx: 0, inputs: OrGateInputs::new(), output: pin }
    }
}

#[derive(Copy, Clone)]
struct OrGateInputs(u64);
impl OrGateInputs {
    fn new() -> Self {
        Self(0)
    }
    #[inline]
    fn is_asserted(self) -> bool {
        self.0 != 0
    }
    #[inline]
    fn is_input_asserted(self, mask: u64) -> bool {
        self.0 & mask != 0
    }
    #[inline]
    fn assert(&mut self, mask: u64) {
        self.0 |= mask;
    }
    #[inline]
    fn deassert(&mut self, mask: u64) {
        self.0 &= !mask;
    }
}

pub struct OrGatePin {
    mask: u64,
    gate: Arc<OrGate>,
}
impl IntrPin for OrGatePin {
    fn assert(&self) {
        self.gate.set_input_state(self.mask, true);
    }
    fn deassert(&self) {
        self.gate.set_input_state(self.mask, false);
    }
    fn pulse(&self) {
        self.gate.pulse_input();
    }
    fn is_asserted(&self) -> bool {
        self.gate.is_input_asserted(self.mask)
    }
    fn import_state(&self, is_asserted: bool) {
        self.gate.import_input_state(self.mask, is_asserted)
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
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Barrier;
    use std::thread;

    pub struct TestPin {
        state: Mutex<TestPinState>,
    }
    impl TestPin {
        pub fn new() -> Self {
            Self {
                state: Mutex::new(TestPinState {
                    is_asserted: false,
                    rises: 0,
                    falls: 0,
                    pulses: 0,
                }),
            }
        }

        // Number of times the pin went from deasserted to asserted.
        pub fn rises(&self) -> usize {
            self.state.lock().unwrap().rises
        }

        // Number of times the pin went from asserted to deasserted.
        pub fn falls(&self) -> usize {
            self.state.lock().unwrap().falls
        }

        // Number of times the pin was pulsed. A pulse also generates a rise
        // and a fall.
        pub fn pulses(&self) -> usize {
            self.state.lock().unwrap().pulses
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

    struct TestPinState {
        is_asserted: bool,
        rises: usize,
        falls: usize,
        pulses: usize,
    }
    impl TestPinState {
        fn assert(&mut self) {
            if !self.is_asserted {
                self.is_asserted = true;
                self.rises += 1;
            }
        }
        fn deassert(&mut self) {
            if self.is_asserted {
                self.is_asserted = false;
                self.falls += 1;
            }
        }
        fn pulse(&mut self) {
            if !self.is_asserted {
                self.rises += 1;
                self.falls += 1;
                self.pulses += 1;
            }
        }
        fn import_state(&mut self, is_asserted: bool) {
            self.is_asserted = is_asserted;
        }
    }

    // Type used to control an OrGate pin during tests.
    type PinSignal = Arc<dyn Fn(&Arc<dyn IntrPin>, usize) + Send + Sync>;

    // Test harness that provides an OrGate connected to a TestPin. Inputs
    // are controlled using PinSignal closures and are synchronized with a
    // shared clock. The PinSignal closure receives the current tick as
    // input.
    //
    // Both the gate and the output pin can be accessed for asserting the
    // overall state of the system.
    struct OrGateDriver {
        gate: Arc<OrGate>,
        output: Arc<TestPin>,
        start_asserted: bool,

        pins: Vec<Arc<dyn IntrPin>>,
        signals: Vec<PinSignal>,
    }
    impl OrGateDriver {
        // Returns a new OrGateDriver with all pins deasserted.
        fn new() -> Self {
            Self::new_inner(false)
        }

        // Returns a new OrGateDriver with all pins asserted.
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

        fn add_signal(&mut self, f: PinSignal) {
            let pin = self.gate.new_input();
            self.pins.push(pin);
            self.signals.push(f);
        }

        fn run(&self, ticks: usize) {
            // The last thread to arrive at a Barrier wakes the other threads
            // and proceeds without locking. This difference can make timing
            // slightly different between inputs.
            //
            // The two Barriers and AtomicBool below are used to achieve a more
            // consistent timing. Threads are controlled by a driver loop that
            // runs in the main thread.
            //
            // Pin threads first block on the `start` `Barrier` and then
            // spinlock on the `go` `AtomicBool` until the driver flips its
            // value. This ensures all threads are awake and ready to poke at
            // their pins.
            //
            // The driver then waits for all threads to complete before
            // flipping `go` again for the next tick.
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
                            // Live-wait for all other threads and the Go!
                            // signal from the driver.
                            start.wait();
                            while !go.load(Ordering::Acquire) {
                                hint::spin_loop();
                            }

                            f(&pin, i);

                            // Block until everyone is ready.
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

        // Assert pin1.
        // Expect gate output to go high.
        pin1.assert();
        assert!(pin1.is_asserted());
        assert!(output_pin.is_asserted());
        assert_eq!(output_pin.rises(), 1);
        assert_eq!(output_pin.falls(), 0);

        // Assert pin2.
        // Expect gate output to remain high and not re-assert.
        pin2.assert();
        assert!(pin2.is_asserted());
        assert!(output_pin.is_asserted());
        assert_eq!(output_pin.rises(), 1);
        assert_eq!(output_pin.falls(), 0);

        // Deassert pin1.
        // Expect gate output to remain high and not re-assert.
        pin1.deassert();
        assert!(!pin1.is_asserted());
        assert!(output_pin.is_asserted());
        assert_eq!(output_pin.rises(), 1);
        assert_eq!(output_pin.falls(), 0);

        // Deassert pin2.
        // Expect gate output to go low.
        pin2.deassert();
        assert!(!pin2.is_asserted());
        assert!(!output_pin.is_asserted());
        assert_eq!(output_pin.rises(), 1);
        assert_eq!(output_pin.falls(), 1);

        // Deassert already deasserted pin3 while gate output is low.
        // Expect no-op.
        pin3.deassert();
        assert!(!pin3.is_asserted());
        assert!(!output_pin.is_asserted());
        assert_eq!(output_pin.rises(), 1);
        assert_eq!(output_pin.falls(), 1);
    }

    #[test]
    fn test_or_gate_pulse() {
        let output_pin = Arc::new(TestPin::new());
        let or_gate = OrGate::new(output_pin.clone());

        let pin1 = or_gate.new_input();
        let pin2 = or_gate.new_input();

        // Pulse while gate output is low.
        // Expect output pulse.
        pin1.pulse();
        assert!(!output_pin.is_asserted());
        assert_eq!(output_pin.rises(), 1);
        assert_eq!(output_pin.falls(), 1);
        assert_eq!(output_pin.pulses(), 1);

        // Pulse while gate output is high.
        // Expect no-op.
        pin1.assert();
        pin2.pulse();
        assert!(output_pin.is_asserted());
        assert_eq!(output_pin.rises(), 2);
        assert_eq!(output_pin.falls(), 1);
        assert_eq!(output_pin.pulses(), 1);

        // Pulse pin that is already high.
        // Expect no-op.
        assert!(pin1.is_asserted());
        pin1.pulse();
        assert!(output_pin.is_asserted());
        assert_eq!(output_pin.rises(), 2);
        assert_eq!(output_pin.falls(), 1);
        assert_eq!(output_pin.pulses(), 1);
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

        // Drop one asserted input.
        // Expect gate output to remain high.
        drop(pin1);
        assert!(output_pin.is_asserted());

        // Drop the other asserted input.
        // Expect gate output to go low.
        drop(pin2);
        assert!(!output_pin.is_asserted());

        // Drop last input.
        // Expect gate output to remain unchanged.
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

        // Import pin states.
        // Expect output state to change without an edge.
        pin1.import_state(false);
        assert!(!output_pin.is_asserted());
        assert_eq!(output_pin.rises(), 0);

        pin2.import_state(true);
        assert!(output_pin.is_asserted());
        assert_eq!(output_pin.rises(), 0);

        pin3.import_state(false);
        assert!(output_pin.is_asserted());
        assert_eq!(output_pin.rises(), 0);
    }

    #[test]
    fn test_or_gate_concurrent_assert() {
        let mut driver = OrGateDriver::new();

        // Assert all pins concurrently.
        // Expect output to go high just once.
        (0..10).for_each(|_| {
            driver.add_signal(Arc::new(move |pin, _| {
                pin.assert();
            }));
        });
        driver.run(100);

        assert!(driver.output.is_asserted());
        assert_eq!(driver.output.rises(), 1);
        assert_eq!(driver.output.falls(), 0);
    }

    #[test]
    fn test_or_gate_concurrent_deassert() {
        let mut driver = OrGateDriver::new_asserted();

        // Deassert all pins concurrently.
        // Expect output to go low just once.
        (0..10).for_each(|_| {
            driver.add_signal(Arc::new(move |pin, _| {
                pin.deassert();
            }));
        });
        driver.run(100);

        assert!(!driver.output.is_asserted());
        assert_eq!(driver.output.rises(), 0);
        assert_eq!(driver.output.falls(), 1);
    }

    #[test]
    fn test_or_gate_concurrent_pulse() {
        let mut driver = OrGateDriver::new();

        // Pulse all pins concurrently.
        // Expect all pulses to be observed by the output.
        (0..10).for_each(|_| {
            driver.add_signal(Arc::new(move |pin, _| {
                pin.pulse();
            }));
        });
        driver.run(100);

        assert!(!driver.output.is_asserted());
        assert_eq!(driver.output.pulses(), 1000);
    }

    // Assert and deassert two pins with square waves in lockstep.
    //
    // Expect output to match the input square wave. The thread that first
    // transition its pin will drive the output while the other one will be a
    // no-op.
    //
    //         ┌─┐ ┌─┐ ┌─┐ ┌─┐
    //   pin1 ─┘ └─┘ └─┘ └─┘ └─
    //         ┌─┐ ┌─┐ ┌─┐ ┌─┐
    //   pin2 ─┘ └─┘ └─┘ └─┘ └─
    //         ┌─┐ ┌─┐ ┌─┐ ┌─┐
    // output ─┘ └─┘ └─┘ └─┘ └─
    #[test]
    fn test_or_gate_concurrent_square_wave() {
        let mut driver = OrGateDriver::new();
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
        assert_eq!(driver.output.rises(), 50);
        assert_eq!(driver.output.falls(), 50);
    }

    // Assert and deassert a pin with a square wave while pulsing another pin.
    //
    // There are four possible outcomes on each tick:
    //   1. pin1 asserts   _after_  pin2 pulses => pulsed while low, output pulses.
    //   2. pin1 deasserts _after_  pin2 pulses => pulsed while high, no-op.
    //   3. pin1 asserts   _before_ pin2 pulses => pulsed while high, no-op.
    //   4. pin1 deasserts _before_ pin2 pulses => pulsed while low, output pulses.
    //
    //         ┌─┐ ┌─┐
    //   pin1 ─┘ └─┘ └──
    //         ▲ ▲ ▲ ▲
    //   pin2 ─┴─┴─┴─┴──
    //         ▲┌┐ ┌─┐▲
    // output ─┴┘└─┘ └┴─
    //
    // In theory, we should expect about half the pulses to cause the output to
    // also pulse but, in practice, the threads will race to lock the gate, so
    // the actual pulse count is non-deterministic and flaky to assert.
    //
    // Expect to at least not have any spurious edges or pulses.
    #[test]
    fn test_or_gate_concurrent_pulse_with_square_wave() {
        let mut driver = OrGateDriver::new();
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
        assert_eq!(driver.output.rises(), driver.output.falls());
        assert!(driver.output.pulses() <= 100);
    }

    #[test]
    #[should_panic(expected = "max number of inputs reached")]
    fn panic_or_gate_max_inputs() {
        let output_pin = Arc::new(TestPin::new());
        let or_gate = OrGate::new(output_pin.clone());
        for _ in 0..100 {
            _ = or_gate.new_input();
        }
    }
}
