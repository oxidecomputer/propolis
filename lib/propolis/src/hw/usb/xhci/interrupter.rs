// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, Weak};

use crate::common::WriteOp;
use crate::hw::pci;
use crate::hw::usb::xhci::bits;
use crate::hw::usb::xhci::registers::InterrupterRegisters;
use crate::hw::usb::xhci::rings::producer::event::{
    Error as TrbRingProducerError, EventInfo, EventRing,
};
use crate::hw::usb::xhci::{RegRWOpValue, NUM_INTRS};
use crate::vmm::{time, MemCtx, VmmHdl};

#[usdt::provider(provider = "propolis")]
mod probes {
    fn xhci_interrupter_pending(intr_num: u16) {}
    fn xhci_interrupter_fired(intr_num: u16) {}
}

pub struct XhciInterrupter {
    number: u16,
    evt_ring_seg_tbl_size: bits::EventRingSegmentTableSize,
    evt_ring_seg_base_addr: bits::EventRingSegmentTableBaseAddress,
    evt_ring: Option<EventRing>,
    interrupts: Arc<(Mutex<InterruptRegulation>, Condvar)>,
    imod_loop: Option<std::thread::JoinHandle<()>>,
    vmm_hdl: Arc<VmmHdl>,
}

struct InterruptRegulation {
    usbcmd_inte: bool,
    any_ip_raised: Weak<AtomicBool>,

    number: u16,
    management: bits::InterrupterManagement,
    moderation: bits::InterrupterModeration,

    // ERDP contains Event Handler Busy
    evt_ring_deq_ptr: bits::EventRingDequeuePointer,

    // IMOD pending has special meaning for INTxPin support,
    // but we still need to
    intr_pending_enable: bool,
    /// IPE, xHCI 1.2 sect 4.17.2
    imod_allow_at: time::VmGuestInstant,

    /// Used for firing PCI interrupts
    pci_intr: XhciPciIntr,

    terminate: bool,

    log: slog::Logger,
}

impl XhciInterrupter {
    pub fn new(
        number: u16,
        pci_intr: XhciPciIntr,
        vmm_hdl: Arc<VmmHdl>,
        any_ip_raised: Weak<AtomicBool>,
        log: slog::Logger,
    ) -> Self {
        let interrupts = Arc::new((
            Mutex::new(InterruptRegulation {
                usbcmd_inte: true,
                any_ip_raised,
                number,
                management: bits::InterrupterManagement::default(),
                moderation: bits::InterrupterModeration::default(),
                evt_ring_deq_ptr: bits::EventRingDequeuePointer(0),
                imod_allow_at: time::VmGuestInstant::now(&vmm_hdl).unwrap(),
                intr_pending_enable: false,
                pci_intr,
                terminate: false,
                log,
            }),
            Condvar::new(),
        ));
        let pair = Arc::downgrade(&interrupts);
        let vmm_hdl_loop = Arc::clone(&vmm_hdl);
        let imod_loop = Some(std::thread::spawn(move || {
            InterruptRegulation::imod_wait_loop(pair, vmm_hdl_loop)
        }));
        Self {
            number,
            evt_ring_seg_tbl_size: bits::EventRingSegmentTableSize(0),
            evt_ring_seg_base_addr:
                bits::EventRingSegmentTableBaseAddress::default(),
            evt_ring: None,
            interrupts,
            imod_loop,
            vmm_hdl,
        }
    }

    pub(super) fn reg_read(
        &self,
        intr_regs: InterrupterRegisters,
    ) -> RegRWOpValue {
        use RegRWOpValue::*;
        match intr_regs {
            InterrupterRegisters::Management => {
                U32(self.interrupts.0.lock().unwrap().management.0)
            }
            InterrupterRegisters::Moderation => {
                let regulation = self.interrupts.0.lock().unwrap();
                let duration =
                    regulation.imod_allow_at.saturating_duration_since(
                        time::VmGuestInstant::now(&self.vmm_hdl).unwrap(),
                    );
                // NOTE: div_duration_f32 not available until 1.80, MSRV is 1.70
                let imodc = (duration.as_secs_f64()
                    / bits::IMOD_TICK.as_secs_f64())
                    as u16;
                U32(regulation.moderation.with_counter(imodc).0)
            }
            InterrupterRegisters::EventRingSegmentTableSize => {
                U32(self.evt_ring_seg_tbl_size.0)
            }
            InterrupterRegisters::EventRingSegmentTableBaseAddress => {
                U64(self.evt_ring_seg_base_addr.address().0)
            }
            InterrupterRegisters::EventRingDequeuePointer => {
                let regulation = self.interrupts.0.lock().unwrap();
                U64(regulation.evt_ring_deq_ptr.0)
            }
        }
    }

    pub(super) fn reg_write(
        &mut self,
        wo: &mut WriteOp,
        intr_regs: InterrupterRegisters,
        memctx: &MemCtx,
    ) -> RegRWOpValue {
        use RegRWOpValue::*;

        let written_value = match intr_regs {
            InterrupterRegisters::Management => {
                let iman = bits::InterrupterManagement(wo.read_u32());
                let mut regulation = self.interrupts.0.lock().unwrap();
                // RW1C
                if iman.pending() {
                    // deassert pin interrupt on Interrupt Pending clear
                    // (only relevant for INTxPin mode)
                    regulation.pci_intr.deassert(self.number);
                    // simulate IMODC being loaded with IMODI when IP cleared to 0
                    // (xHCI 1.2 sect 5.5.2.2)
                    regulation.imod_allow_at =
                        time::VmGuestInstant::now(&self.vmm_hdl)
                            .unwrap()
                            .checked_add(
                                regulation.moderation.interval_duration(),
                            )
                            .unwrap();
                    regulation.management.set_pending(false);
                }
                // RW
                regulation.management.set_enable(iman.enable());
                self.interrupts.1.notify_one();
                // remainder of register is reserved
                U32(iman.0)
            }
            InterrupterRegisters::Moderation => {
                let mut regulation = self.interrupts.0.lock().unwrap();
                regulation.moderation =
                    bits::InterrupterModeration(wo.read_u32());
                // emulating setting the value of IMODC, which counts down to zero.
                if let Some(inst) = time::VmGuestInstant::now(&self.vmm_hdl)
                    .unwrap()
                    .checked_add(
                        bits::IMOD_TICK
                            * regulation.moderation.counter() as u32,
                    )
                {
                    regulation.imod_allow_at = inst;
                    self.interrupts.1.notify_one();
                }
                U32(regulation.moderation.0)
            }
            InterrupterRegisters::EventRingSegmentTableSize => {
                self.evt_ring_seg_tbl_size =
                    bits::EventRingSegmentTableSize(wo.read_u32());
                U32(self.evt_ring_seg_tbl_size.0)
            }
            InterrupterRegisters::EventRingSegmentTableBaseAddress => {
                self.evt_ring_seg_base_addr =
                    bits::EventRingSegmentTableBaseAddress(wo.read_u64());
                U64(self.evt_ring_seg_base_addr.0)
            }
            InterrupterRegisters::EventRingDequeuePointer => {
                let erdp = bits::EventRingDequeuePointer(wo.read_u64());
                let mut regulation = self.interrupts.0.lock().unwrap();
                regulation.evt_ring_deq_ptr.set_dequeue_erst_segment_index(
                    erdp.dequeue_erst_segment_index(),
                );
                regulation.evt_ring_deq_ptr.set_pointer(erdp.pointer());
                // RW1C
                if erdp.handler_busy() {
                    regulation.evt_ring_deq_ptr.set_handler_busy(false);
                }
                self.interrupts.1.notify_one();
                U64(erdp.0)
            }
        };

        let mut regulation = self.interrupts.0.lock().unwrap();
        let erstba = self.evt_ring_seg_base_addr.address();
        let erstsz = self.evt_ring_seg_tbl_size.size() as usize;
        let erdp = regulation.evt_ring_deq_ptr.pointer();

        if let Some(event_ring) = &mut self.evt_ring {
            match intr_regs {
                InterrupterRegisters::EventRingSegmentTableSize
                | InterrupterRegisters::EventRingSegmentTableBaseAddress => {
                    if let Err(e) =
                        event_ring.update_segment_table(erstba, erstsz, &memctx)
                    {
                        slog::error!(
                            regulation.log,
                            "Event Ring Segment Table update failed: {e}"
                        );
                    }
                }
                InterrupterRegisters::EventRingDequeuePointer => {
                    let empty_before = event_ring.is_empty();
                    event_ring.update_dequeue_pointer(erdp);
                    if !empty_before && event_ring.is_empty() {
                        // IPE should be set to 0 "when the Event Ring transitions to empty"
                        regulation.intr_pending_enable = false;
                        self.interrupts.1.notify_one();
                    }
                }
                _ => (),
            }
        } else {
            match intr_regs {
                InterrupterRegisters::EventRingSegmentTableBaseAddress => {
                    match EventRing::new(erstba, erstsz, erdp, &memctx) {
                        Ok(evt_ring) => self.evt_ring = Some(evt_ring),
                        Err(e) => {
                            slog::error!(
                                regulation.log,
                                "Event Ring Segment Table update failed: {e}"
                            );
                        }
                    }
                }
                _ => (),
            }
        }

        written_value
    }

    // returns Ok when an event was enqueued and an interrupt was fired
    pub fn enqueue_event(
        &mut self,
        event_info: EventInfo,
        memctx: &MemCtx,
        block_event_interrupt: bool,
    ) -> Result<(), TrbRingProducerError> {
        if let Some(evt_ring) = self.evt_ring.as_mut() {
            let mut regulation = self.interrupts.0.lock().unwrap();

            if let Err(e) = evt_ring.enqueue(event_info.into(), &memctx) {
                slog::error!(
                    regulation.log,
                    "failed to enqueue Event TRB: {e}"
                );
                return Err(e);
            }
            // check imod/iman for when to fire pci intr
            if !block_event_interrupt {
                regulation.intr_pending_enable = true;
                self.interrupts.1.notify_one();
                let intr_num = self.number;
                probes::xhci_interrupter_pending!(move || (intr_num));
            }

            Ok(())
        } else {
            Err(TrbRingProducerError::Interrupter)
        }
    }

    pub fn set_pci_intr_mode(
        &mut self,
        mode: pci::IntrMode,
        pci_state: &pci::DeviceState,
    ) {
        self.interrupts.0.lock().unwrap().pci_intr.set_mode(mode, pci_state);
        self.interrupts.1.notify_one();
    }

    pub fn set_usbcmd_inte(&self, usbcmd_inte: bool) {
        self.interrupts.0.lock().unwrap().usbcmd_inte = usbcmd_inte;
        self.interrupts.1.notify_one();
    }
}

impl Drop for XhciInterrupter {
    fn drop(&mut self) {
        self.interrupts.0.lock().unwrap().terminate = true;
        self.interrupts.1.notify_one();
        // Thread::join requires ownership
        self.imod_loop.take().unwrap().join().ok();
    }
}

// xHCI 1.2 sect 4.17.5:
// """
// The IPE flag of an Interrupter is managed as follows:
// - IPE shall be cleared to 0:
//   - When the Event Ring is initialized.
//   - If the Event Ring transitions to empty.
// - When an Event TRB is inserted on the Event Ring and BEI = 0 then:
//   - IPE shall be set to 1.
// Note: Only Normal, Isoch, and Event Data TRBs support a BEI flag.
//
// The Interrupt Pending (IP) flag of an Interrupter shall be managed as follows:
// - When IPE transitions to 1:
//   - If Interrupt Moderation Counter (IMODC) = 0 and Event Handler Busy (EHB) = 0,
//     then IP shall be set to 1.
// - When IMODC transitions to 0:
//   - If EHB = 0 and IPE = 1, then IP shall be set to 1.
//
// - If MSI or MSI-X interrupts are enabled, IP shall be cleared to 0 automatically when
//   the PCI Dword write generated by the Interrupt assertion is complete.
// - If PCI Pin Interrupts are enabled then, IP shall be cleared to 0 by software.
// """

impl InterruptRegulation {
    // handling IMODI / IMODC / IP / IE.
    // xHCI 1.2 figure 4-22
    fn imod_wait_loop(
        pair: Weak<(Mutex<Self>, Condvar)>,
        vmm_hdl: Arc<VmmHdl>,
    ) {
        while let Some(pair) = pair.upgrade() {
            let (regulation, cvar) = &*pair;

            let guard = regulation.lock().unwrap();

            // wait for simulated IMODC to tick down to 0
            let Ok(now) = time::VmGuestInstant::now(&vmm_hdl) else { break };
            let imod_allow_at = guard.imod_allow_at;
            let timeout = guard.imod_allow_at.saturating_duration_since(now);
            // the golden path here *is* for this to time out - we're only
            // checking for writes to IMODC during our wait
            let (guard, timeout_result) = cvar
                .wait_timeout_while(guard, timeout, |ir| {
                    ir.imod_allow_at == imod_allow_at
                })
                .unwrap();
            if !timeout_result.timed_out() {
                // IMODC written, restart loop with new value of imod_allow_at
                continue;
            }

            let mut guard = cvar
                .wait_while(guard, |ir| {
                    !(ir.terminate
                        || (ir.usbcmd_inte
                            && ir.management.enable()
                            && ir.intr_pending_enable
                            && !ir.evt_ring_deq_ptr.handler_busy()))
                })
                .unwrap();

            if guard.terminate || vmm_hdl.is_destroyed() {
                break;
            }

            let Some(ip_raised) = guard.any_ip_raised.upgrade() else { break };
            guard.management.set_pending(true);
            ip_raised.store(true, Ordering::Release);

            guard.evt_ring_deq_ptr.set_handler_busy(true);
            guard.pci_intr.fire_interrupt(guard.number);
            // IP flag cleared by the completion of PCI write
            // (xHCI 1.2 fig 4-22 description)
            guard.management.set_pending(false);
            // load counter with interval
            guard.imod_allow_at = time::VmGuestInstant::now(&vmm_hdl)
                .unwrap()
                .checked_add(guard.moderation.interval_duration())
                .unwrap();
        }
    }
}

pub struct XhciPciIntr {
    msix_hdl: Option<pci::MsixHdl>,
    pin: Option<Arc<dyn crate::intr_pins::IntrPin>>,
    pci_intr_mode: pci::IntrMode,
    log: slog::Logger,
}

impl XhciPciIntr {
    pub fn set_mode(
        &mut self,
        mode: pci::IntrMode,
        pci_state: &pci::DeviceState,
    ) {
        slog::debug!(self.log, "xHC set interrupt mode to {mode:?}");
        self.pci_intr_mode = mode;
        self.msix_hdl = pci_state.msix_hdl();
        self.pin = pci_state.lintr_pin();
    }

    // xHCI 1.2 sect 4.17
    pub fn fire_interrupt(&self, interrupter_num: u16) {
        match self.pci_intr_mode {
            pci::IntrMode::Disabled => {
                slog::error!(
                    self.log,
                    "xHC fired PCIe interrupt, but IntrMode was Disabled"
                );
            }
            pci::IntrMode::INTxPin => {
                // NOTE: Only supports one interrupter, per xHCI 1.2 sect 4.17.
                // If changing number of interrupters, either remove support for INTxPin here,
                // or implement disabling all but the first interrupter everywhere else.
                const _: () = const { assert!(NUM_INTRS <= 1) };
                if interrupter_num == 0 {
                    if let Some(pin) = &self.pin {
                        slog::debug!(self.log, "xHC interrupter asserting");
                        pin.assert();
                        probes::xhci_interrupter_fired!(|| interrupter_num);
                    } else {
                        slog::error!(
                            self.log,
                            "xHC in INTxPin mode with no pin"
                        );
                    }
                } else {
                    slog::error!(
                        self.log,
                        "xHC INTxPin tried to fire for non-zero Interrupter"
                    );
                }
            }
            pci::IntrMode::Msix => {
                if let Some(msix_hdl) = self.msix_hdl.as_ref() {
                    msix_hdl.fire(interrupter_num);
                    probes::xhci_interrupter_fired!(|| interrupter_num);
                    slog::debug!(
                        self.log,
                        "xHC interrupter firing: {interrupter_num}"
                    );
                } else {
                    slog::error!(
                        self.log,
                        "xHC interrupter missing MSI-X handle"
                    );
                }
            }
        }
    }

    pub fn deassert(&self, interrupter_num: u16) {
        // only relevant for INTxPin mode
        if let Some(pin) = &self.pin {
            if interrupter_num == 0 {
                pin.deassert();
            } else {
                slog::error!(
                    self.log,
                    "tried to deassert INTxPin of non-zero interrupter number"
                );
            }
        }
    }

    pub fn new(pci_state: &pci::DeviceState, log: slog::Logger) -> Self {
        Self {
            msix_hdl: pci_state.msix_hdl(),
            pin: pci_state.lintr_pin(),
            pci_intr_mode: pci_state.get_intr_mode(),
            log,
        }
    }
}

// migration

impl XhciInterrupter {
    pub fn export(
        &self,
    ) -> Result<migrate::XhciInterrupterV1, crate::migrate::MigrateStateError>
    {
        let XhciInterrupter {
            number,
            evt_ring_seg_tbl_size,
            evt_ring_seg_base_addr,
            evt_ring,
            interrupts,
            imod_loop: _,
            vmm_hdl: _,
        } = self;
        let guard = interrupts.0.lock().unwrap();
        let cvar = &interrupts.1;

        let payload = migrate::XhciInterrupterV1 {
            number: *number,
            evt_ring_seg_tbl_size: evt_ring_seg_tbl_size.0,
            evt_ring_seg_base_addr: evt_ring_seg_base_addr.0,
            evt_ring: evt_ring.as_ref().map(From::from),
            interrupts: migrate::InterruptRegulationV1 {
                usbcmd_inte: guard.usbcmd_inte,
                number: guard.number,
                management: guard.management.0,
                moderation: guard.moderation.0,
                evt_ring_deq_ptr: guard.evt_ring_deq_ptr.0,
                intr_pending_enable: guard.intr_pending_enable,
                imod_allow_at: guard.imod_allow_at,
                terminate: guard.terminate,
            },
        };
        cvar.notify_one();
        Ok(payload)
    }

    pub fn import(
        &mut self,
        value: migrate::XhciInterrupterV1,
    ) -> Result<(), crate::migrate::MigrateStateError> {
        let migrate::XhciInterrupterV1 {
            number,
            evt_ring_seg_tbl_size,
            evt_ring_seg_base_addr,
            evt_ring,
            interrupts:
                migrate::InterruptRegulationV1 {
                    usbcmd_inte,
                    number: _,
                    management,
                    moderation,
                    evt_ring_deq_ptr,
                    intr_pending_enable,
                    imod_allow_at,
                    terminate,
                },
        } = value;

        let mut guard = self.interrupts.0.lock().unwrap();
        let cvar = &self.interrupts.1;

        self.number = number;
        self.evt_ring_seg_tbl_size =
            bits::EventRingSegmentTableSize(evt_ring_seg_tbl_size);
        self.evt_ring_seg_base_addr =
            bits::EventRingSegmentTableBaseAddress(evt_ring_seg_base_addr);
        self.evt_ring = evt_ring.as_ref().map(From::from);
        guard.usbcmd_inte = usbcmd_inte;
        guard.number = number;
        guard.management = bits::InterrupterManagement(management);
        guard.moderation = bits::InterrupterModeration(moderation);
        guard.evt_ring_deq_ptr =
            bits::EventRingDequeuePointer(evt_ring_deq_ptr);
        guard.intr_pending_enable = intr_pending_enable;
        guard.imod_allow_at = imod_allow_at;
        guard.terminate = terminate;

        cvar.notify_one();
        Ok(())
    }
}

pub mod migrate {
    use crate::{hw::usb::xhci::rings::producer::event::migrate::*, vmm::time};
    use serde::{Deserialize, Serialize};

    #[derive(Deserialize, Serialize)]
    pub struct XhciInterrupterV1 {
        pub number: u16,
        pub evt_ring_seg_tbl_size: u32,
        pub evt_ring_seg_base_addr: u64,
        pub evt_ring: Option<EventRingV1>,
        pub interrupts: InterruptRegulationV1,
    }

    #[derive(Deserialize, Serialize)]
    pub struct InterruptRegulationV1 {
        pub usbcmd_inte: bool,
        pub number: u16,
        pub management: u32,
        pub moderation: u32,
        pub evt_ring_deq_ptr: u64,
        pub intr_pending_enable: bool,
        pub imod_allow_at: time::VmGuestInstant,
        pub terminate: bool,
    }
}
