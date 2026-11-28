// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Simulator for how guests handle ACPI GPEs with ACPICA.
//!
//! The implementation is based on ACPICA source code[0] and tries to match its
//! behaviour as closely as possible. Method names, some comments, and
//! programming style are also kept similar to the original source for easier
//! matching. Some kept comments may not match the actual behaviour of the
//! simulator. When that happens, a `NOTE(propolis)` provides the
//! simulator-specific explanation.
//!
//! A few notable gaps that are not implemented:
//!
//! - ACPI method handling is done asynchronously in a real guest, meaning
//!   that the method can run while other GPEs are still being handled. To
//!   facilitate tests, ACPI method execution must be triggered synchronously
//!   with [`AcpicaSim::run_queued_methods`].
//! - Interrupt masking and entering and leaving sleep states are out of
//!   scope.
//!
//! [0]: https://github.com/open-acpica/acpica/blob/e85aa3bebaae81ed1e6d163511ac7e1eafe732cf/source/components/events

use std::collections::BTreeMap;
use std::sync::Once;

use super::test::*;
use super::*;

static REGISTER_PROBES: Once = Once::new();

/// Simulator that makes port I/O operations to a GPE register block that
/// resembles how a guest that uses ACPICA behaves.
///
/// The simulator can register the DTrace probes used to track GPEs. Use
/// `scripts/gpe-trace.d` to visualize events. Ensure tests call
/// [`AcpicaSim::new`] with `register_probes` set to `true`.
///
/// ```text
/// $ cargo test -p propolis --lib --no-run
/// $ pfexec dtrace -Z -s scripts/gpe-trace.d -c "target/debug/deps/propolis-<HASH> firmware::acpi::gpe::test::<TEST NAME> --exact"
/// ```
///
/// To help inspect internal state, the simulator is single-threaded and is
/// split into phases.
///
/// 1. Environment setup. Use [`AcpicaSim::new`] to create the simulator and
///    [`AcpicaSim::add_event`] to populate events.
/// 2. Boot. Call [`AcpicaSim::initialize`] to simulate the operations guests
///    do on boot, such as enabling the GPEs with corresponding `_Exx` or
///    `_Lxx` methods.
/// 3. Activate events. Call [`RegisterBlock::trigger`] or
///    [`RegisterBlock::set_level`] to set bits in `GPEx_STS`.
/// 4. Send interrupt. The simulator is not directly connected to the SCI, so
///    call [`AcpicaSim::interrupt`] to simulate the guest receiving the
///    interrupt.
/// 5. Run event handlers. Real guests enqueue event handlers for async
///    execution, but in the simulator they must be called manually using
///    [`AcpicaSim::run_queued_methods`].
pub struct AcpicaSim<'a> {
    reg_block: &'a RegisterBlock,
    events: BTreeMap<u8, Event>,
    event_handlers: BTreeMap<u8, Box<dyn Fn() + 'a>>,

    methods_queue: Vec<Event>,
    enable_queue: Vec<Event>,
}
impl<'a> AcpicaSim<'a> {
    /// Creates a new simulator for `reg_block`. If `register_probes` is true,
    /// the DTrace probes used to track GPEs are registered.
    pub fn new(reg_block: &'a RegisterBlock, register_probes: bool) -> Self {
        if register_probes {
            REGISTER_PROBES.call_once(|| {
                crate::usdt::register_probes().unwrap();
            });
        }

        Self {
            reg_block,
            events: BTreeMap::new(),
            event_handlers: BTreeMap::new(),
            methods_queue: Vec::new(),
            enable_queue: Vec::new(),
        }
    }

    /// Adds `ev` to the simulator. `f` can be used to simulate an ACPI method
    /// that handles the event.
    pub fn add_event(&mut self, ev: Event, f: Option<Box<dyn Fn() + 'a>>) {
        let k = ev.number();
        let handler = match f {
            Some(f) => f,
            None => Box::new(|| {}),
        };

        assert!(self.reg_block.gpe_range.contains(&k));
        assert!(self.events.insert(k, ev).is_none());
        assert!(self.event_handlers.insert(k, handler).is_none());
    }

    /// Simulates the guest boot and initializes the GPEs registered with
    /// [`AcpicaSim::add_event`]. Loosely based on Linux's `acpi_init`[0]
    /// function.
    ///
    /// [0]: https://github.com/torvalds/linux/blob/17fa96334e15c6b38e8d9dd3627f3a6c5ca034f8/drivers/acpi/bus.c#L1454
    pub fn initialize(&mut self) {
        self.acpi_enable_subsystem();
        self.acpi_ev_initialize_gpe_block();
        self.acpi_ev_gpe_detect();
    }

    /// Simulates the guest receiving an SCI interrupt.
    pub fn interrupt(&mut self) {
        self.acpi_ev_gpe_detect();
    }

    /// Runs the methods associated with GPEs that had `GPEx_STS & GPEx_EN !=
    /// 0` when the interrupt was handled and finishes the GPE execution by W1C
    /// level-triggered GPEs and re-enables GPEs that were disabled.
    pub fn run_queued_methods(&mut self) {
        for gpe_event_info in std::mem::take(&mut self.methods_queue) {
            self.acpi_ev_asynch_execute_gpe_method(gpe_event_info);
        }

        for gpe_event_info in std::mem::take(&mut self.enable_queue) {
            self.acpi_ev_asynch_enable_gpe(gpe_event_info);
        }
    }

    fn len(&self) -> usize {
        self.reg_block.inner.lock().unwrap().sts.len()
    }

    fn base(&self) -> u8 {
        *self.reg_block.gpe_range.start()
    }

    // AcpiEnableSubsystem
    // https://github.com/open-acpica/acpica/blob/e85aa3bebaae81ed1e6d163511ac7e1eafe732cf/source/components/utilities/utxfinit.c#L125
    fn acpi_enable_subsystem(&self) {
        // Initialize ACPI Event handling (Fixed and General Purpose)
        //
        // NOTE(propolis): the simulator doesn't model fixed events.
        self.acpi_ev_initialize_events();
    }

    // AcpiEvInitializeEvents
    // https://github.com/open-acpica/acpica/blob/e85aa3bebaae81ed1e6d163511ac7e1eafe732cf/source/components/events/evevent.c#L47
    fn acpi_ev_initialize_events(&self) {
        // Initialize the Fixed and General Purpose Events. This is done prior to
        // enabling SCIs to prevent interrupts from occurring before the handlers
        // are installed.
        //
        // NOTE(propolis): the simulator doesn't model fixed events.
        self.acpi_ev_gpe_initialize();
    }

    // AcpiEvGpeInitialize
    // https://github.com/open-acpica/acpica/blob/e85aa3bebaae81ed1e6d163511ac7e1eafe732cf/source/components/events/evgpeinit.c#L54
    fn acpi_ev_gpe_initialize(&self) {
        // Determine the maximum GPE number for this machine.
        //
        // Note: both GPE0 and GPE1 are optional, and either can exist without
        // the other.
        //
        // If EITHER the register length OR the block address are zero, then that
        // particular block is not supported.
        //
        // NOTE(propolis): we currently ignore GPE1.

        // GPE block 0 exists (has both length and address > 0)
        //
        // NOTE(propolis): we currently assume that this is always the case and
        // ignore GPE1.

        // Install GPE Block 0
        self.acpi_ev_create_gpe_block();
    }

    // AcpiEvCreateGpeBlock
    // https://github.com/open-acpica/acpica/blob/e85aa3bebaae81ed1e6d163511ac7e1eafe732cf/source/components/events/evgpeblk.c#L341
    fn acpi_ev_create_gpe_block(&self) {
        // Create the RegisterInfo and EventInfo sub-structures
        // Note: disables and clears all GPEs in the block
        self.acpi_ev_create_gpe_info_blocks();

        // Find all GPE methods (_Lxx or_Exx) for this block
        //
        // NOTE(propolis): ACPICA traverses the methods in the order defined in
        // the ACPI tables namespace, but the simulator iterates in `BTreeMap`
        // order, so DTrace events may not happen in the same order as in a
        // real guest.
        for ev in self.events.values() {
            self.acpi_ev_match_gpe_method(*ev);
        }
    }

    // AcpiEvMatchGpeMethod
    // https://github.com/open-acpica/acpica/blob/e85aa3bebaae81ed1e6d163511ac7e1eafe732cf/source/components/events/evgpeinit.c#L297
    fn acpi_ev_match_gpe_method(&self, gpe_event_info: Event) {
        // Disable the GPE in case it's been enabled already.
        self.acpi_hw_low_set_gpe(
            gpe_event_info,
            AcpicaSimAction::AcpiGpeDisable,
        );
    }

    // AcpiEvCreateGpeInfoBlocks
    // https://github.com/open-acpica/acpica/blob/e85aa3bebaae81ed1e6d163511ac7e1eafe732cf/source/components/events/evgpeblk.c#L201
    fn acpi_ev_create_gpe_info_blocks(&self) {
        for i in 0..self.len() {
            // Disable all GPEs within this register
            self.acpi_hw_write(0x00, Register::En(i));

            // Clear any pending GPE events within this register
            self.acpi_hw_write(0xFF, Register::Sts(i));
        }
    }

    // AcpiEvInitializeGpeBlock
    // https://github.com/open-acpica/acpica/blob/e85aa3bebaae81ed1e6d163511ac7e1eafe732cf/source/components/events/evgpeblk.c#L451
    fn acpi_ev_initialize_gpe_block(&self) {
        // Enable all GPEs that have a corresponding method and have the
        // ACPI_GPE_CAN_WAKE flag unset. Any other GPEs within this block
        // must be enabled via the acpi_enable_gpe() interface.
        //
        // NOTE(propolis): this test model ignores wake and mask flags.
        for i in 0..self.len() {
            for j in 0..8 {
                // Get the info block for this particular GPE
                let gpe_index = ((i * 8) + j) as u8;
                let gpe_number = self.base() + gpe_index;

                // Ignore GPEs that have no corresponding _Lxx/_Exx method
                // and GPEs that are used to wake the system
                let ev = match self.events.get(&gpe_number) {
                    Some(ev) => *ev,
                    None => continue,
                };

                self.acpi_ev_add_gpe_reference(ev, false);
            }
        }
    }

    // AcpiEvAddGpeReference
    // https://github.com/open-acpica/acpica/blob/e85aa3bebaae81ed1e6d163511ac7e1eafe732cf/source/components/events/evgpe.c#L191
    fn acpi_ev_add_gpe_reference(
        &self,
        gpe_event_info: Event,
        clear_on_enable: bool,
    ) {
        if clear_on_enable {
            self.acpi_hw_clear_gpe(gpe_event_info);
        }

        self.acpi_ev_enable_gpe(gpe_event_info);
    }

    // AcpiEvEnableGpe
    // https://github.com/open-acpica/acpica/blob/e85aa3bebaae81ed1e6d163511ac7e1eafe732cf/source/components/events/evgpe.c#L96
    fn acpi_ev_enable_gpe(&self, gpe_event_info: Event) {
        // Enable the requested GPE
        self.acpi_hw_low_set_gpe(
            gpe_event_info,
            AcpicaSimAction::AcpiGpeEnable,
        );
    }

    // AcpiEvGpeDetect
    // https://github.com/open-acpica/acpica/blob/e85aa3bebaae81ed1e6d163511ac7e1eafe732cf/source/components/events/evgpe.c#L403
    fn acpi_ev_gpe_detect(&mut self) {
        for i in 0..self.len() {
            // Optimization: If there are no GPEs enabled within this
            // register, we can safely ignore the entire register.
            if self.reg_block.inner.lock().unwrap().en[i] == 0 {
                continue;
            }

            // Now look at the individual GPEs in this byte register
            for j in 0..8 {
                // Detect and dispatch one GPE bit
                let gpe_number = (j + 8 * i) as u8 + self.base();
                let ev = match self.events.get(&gpe_number) {
                    Some(ev) => *ev,
                    None => {
                        // NOTE(propolis): ACPICA always iterates over all
                        // bits. Use a placeholder GPE to trigger
                        // `acpi_ev_detect_gpe` even if the bit doesn't have a
                        // GPE registered.
                        Event::new(gpe_number, Trigger::Edge)
                    }
                };
                self.acpi_ev_detect_gpe(ev);
            }
        }
    }

    // AcpiEvDetectGpe
    // https://github.com/open-acpica/acpica/blob/e85aa3bebaae81ed1e6d163511ac7e1eafe732cf/source/components/events/evgpe.c#L794
    fn acpi_ev_detect_gpe(&mut self, gpe_event_info: Event) {
        let reg_idx = reg_index(gpe_event_info, self.base());

        // Get the register bitmask for this GPE
        let register_bit = self.acpi_hw_get_gpe_register_bit(gpe_event_info);

        // GPE currently enabled (enable bit == 1)?
        let mut enable_reg = 0;
        self.acpi_hw_read(&mut enable_reg, Register::En(reg_idx));

        // GPE currently active (status bit == 1)?
        let mut status_reg = 0;
        self.acpi_hw_read(&mut status_reg, Register::Sts(reg_idx));

        // Check if there is anything active at all in this GPE
        let enabled_status_byte = status_reg & enable_reg;
        if !(enabled_status_byte & register_bit != 0) {
            return;
        }

        // Found an active GPE
        self.acpi_ev_gpe_dispatch(gpe_event_info);
    }

    // AcpiEvGpeDispatch
    // https://github.com/open-acpica/acpica/blob/e85aa3bebaae81ed1e6d163511ac7e1eafe732cf/source/components/events/evgpe.c#L929
    fn acpi_ev_gpe_dispatch(&mut self, gpe_event_info: Event) {
        // Always disable the GPE so that it does not keep firing before
        // any asynchronous activity completes (either from the execution
        // of a GPE method or an asynchronous GPE handler.)
        //
        // If there is no handler or method to run, just disable the
        // GPE and leave it disabled permanently to prevent further such
        // pointless events from firing.
        //
        // NOTE(propolis): the simulator assumes all events have a
        // corresponding handler.
        self.acpi_hw_low_set_gpe(
            gpe_event_info,
            AcpicaSimAction::AcpiGpeDisable,
        );

        // If edge-triggered, clear the GPE status bit now. Note that
        // level-triggered events are cleared after the GPE is serviced.
        if gpe_event_info.trigger() == Trigger::Edge {
            self.acpi_hw_clear_gpe(gpe_event_info);
        }

        // Execute the method associated with the GPE
        // NOTE: Level-triggered GPEs are cleared after the method completes.
        self.acpi_os_execute(
            AcpicaOsExecuteType::OslGpeHandler,
            gpe_event_info,
        );
    }

    // AcpiOsExecute
    //
    // NOTE(propolis): the method name is kept to match references in callers.
    // The actual implementation calls on the guest OS to queue functions for
    // later or concurrent execution.
    fn acpi_os_execute(
        &mut self,
        t: AcpicaOsExecuteType,
        gpe_event_info: Event,
    ) {
        match t {
            AcpicaOsExecuteType::OslGpeHandler => {
                self.methods_queue.push(gpe_event_info)
            }
            AcpicaOsExecuteType::OslNotifyHandler => {
                self.enable_queue.push(gpe_event_info)
            }
        }
    }

    // AcpiEvAsynchExecuteGpeMethod
    // https://github.com/open-acpica/acpica/blob/e85aa3bebaae81ed1e6d163511ac7e1eafe732cf/source/components/events/evgpe.c#L508
    fn acpi_ev_asynch_execute_gpe_method(&mut self, gpe_event_info: Event) {
        // Invoke the GPE Method (_Lxx, _Exx) i.e., evaluate the
        // _Lxx/_Exx control method that corresponds to this GPE
        self.acpi_ns_evaluate(gpe_event_info);

        // Defer enabling of GPE until all notify handlers are done
        self.acpi_os_execute(
            AcpicaOsExecuteType::OslNotifyHandler,
            gpe_event_info,
        );
    }

    // AcpiNsEvaluate
    // https://github.com/open-acpica/acpica/blob/e85aa3bebaae81ed1e6d163511ac7e1eafe732cf/source/components/namespace/nseval.c#L191
    fn acpi_ns_evaluate(&self, gpe_event_info: Event) {
        // Simulate guest running the ACPI method.
        let f = self.event_handlers.get(&gpe_event_info.number()).unwrap();
        f();
    }

    // AcpiEvAsynchEnableGpe
    // https://github.com/open-acpica/acpica/blob/e85aa3bebaae81ed1e6d163511ac7e1eafe732cf/source/components/events/evgpe.c#L713
    fn acpi_ev_asynch_enable_gpe(&self, gpe_event_info: Event) {
        self.acpi_ev_finish_gpe(gpe_event_info);
    }

    // AcpiEvFinishGpe
    // https://github.com/open-acpica/acpica/blob/e85aa3bebaae81ed1e6d163511ac7e1eafe732cf/source/components/events/evgpe.c#L742
    fn acpi_ev_finish_gpe(&self, gpe_event_info: Event) {
        if gpe_event_info.trigger() == Trigger::Level {
            // GPE is level-triggered, we clear the GPE status bit after
            // handling the event.
            self.acpi_hw_clear_gpe(gpe_event_info);
        }

        // Enable this GPE, conditionally. This means that the GPE will
        // only be physically enabled if the EnableMask bit is set
        // in the EventInfo.
        self.acpi_hw_low_set_gpe(
            gpe_event_info,
            AcpicaSimAction::AcpiGpeConditionalEnable,
        );
    }

    // AcpiHwLowSetGpe
    // https://github.com/open-acpica/acpica/blob/e85aa3bebaae81ed1e6d163511ac7e1eafe732cf/source/components/hardware/hwgpe.c#L76
    fn acpi_hw_low_set_gpe(
        &self,
        gpe_event_info: Event,
        action: AcpicaSimAction,
    ) {
        let reg_idx = reg_index(gpe_event_info, self.base());

        // Get current value of the enable register that contains this GPE
        let mut enable_mask = 0;
        self.acpi_hw_read(&mut enable_mask, Register::En(reg_idx));

        // Set or clear just the bit that corresponds to this GPE
        let register_bit = self.acpi_hw_get_gpe_register_bit(gpe_event_info);

        match action {
            // NOTE(propolis): we assume all GPEs are enabled.
            AcpicaSimAction::AcpiGpeConditionalEnable
            | AcpicaSimAction::AcpiGpeEnable => {
                Self::acpi_set_bit(&mut enable_mask, register_bit);
            }
            AcpicaSimAction::AcpiGpeDisable => {
                Self::acpi_clear_bit(&mut enable_mask, register_bit);
            }
        }

        // Write the updated enable mask
        self.acpi_hw_write(enable_mask, Register::En(reg_idx));
    }

    // AcpiHwClearGpe
    // https://github.com/open-acpica/acpica/blob/e85aa3bebaae81ed1e6d163511ac7e1eafe732cf/source/components/hardware/hwgpe.c#L160
    fn acpi_hw_clear_gpe(&self, gpe_event_info: Event) {
        let reg_idx = reg_index(gpe_event_info, self.base());

        // Write a one to the appropriate bit in the status register to
        // clear this GPE.
        let register_bit = self.acpi_hw_get_gpe_register_bit(gpe_event_info);
        self.acpi_hw_write(register_bit, Register::Sts(reg_idx));
    }

    // AcpiHwGetGpeRegisterBit
    // https://github.com/open-acpica/acpica/blob/e85aa3bebaae81ed1e6d163511ac7e1eafe732cf/source/components/hardware/hwgpe.c#L51
    fn acpi_hw_get_gpe_register_bit(&self, gpe_event_info: Event) -> u8 {
        reg_mask(gpe_event_info, self.base())
    }

    // AcpiHwRead
    //
    // NOTE(propolis): the actual implementation calls the OS to read the I/O
    // port.
    fn acpi_hw_read(&self, value: &mut u8, reg: Register) {
        *value = pio_read(self.reg_block, reg);
    }

    // AcpiHwWrite
    //
    // NOTE(propolis): the actual implementation calls the OS to write to the
    // I/O port.
    fn acpi_hw_write(&self, value: u8, reg: Register) {
        pio_write(self.reg_block, reg, value);
    }

    // ACPI_SET_BIT
    // https://github.com/open-acpica/acpica/blob/e85aa3bebaae81ed1e6d163511ac7e1eafe732cf/source/include/actypes.h#L503
    fn acpi_set_bit(target: &mut u8, bit: u8) {
        *target |= bit
    }

    // ACPI_CLEAR_BIT
    // https://github.com/open-acpica/acpica/blob/e85aa3bebaae81ed1e6d163511ac7e1eafe732cf/source/include/actypes.h#L504
    fn acpi_clear_bit(target: &mut u8, bit: u8) {
        *target &= !bit
    }
}

enum AcpicaSimAction {
    AcpiGpeConditionalEnable,
    AcpiGpeEnable,
    AcpiGpeDisable,
}

enum AcpicaOsExecuteType {
    OslGpeHandler,
    OslNotifyHandler,
}
