// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use crate::common::WriteOp;
use crate::hw::pci;
use crate::hw::usb::xhci::bits;
use crate::hw::usb::xhci::registers::InterrupterRegisters;
use crate::hw::usb::xhci::rings::producer::event::{
    Error as TrbRingProducerError, EventInfo, EventRing,
};
use crate::hw::usb::xhci::{RegRWOpValue, NUM_INTRS};
use crate::vmm::MemCtx;

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
}

struct InterruptRegulation {
    usbcmd_inte: bool,

    number: u16,
    management: bits::InterrupterManagement,
    moderation: bits::InterrupterModeration,

    // ERDP contains Event Handler Busy
    evt_ring_deq_ptr: bits::EventRingDequeuePointer,

    // IMOD pending has special meaning for INTxPin support,
    // but we still need to
    intr_pending_enable: bool,
    /// IPE, xHCI 1.2 sect 4.17.2
    imod_allow_at: Instant,
    imod_timeouts_consecutive: u32,

    /// Used for firing PCI interrupts
    pci_intr: XhciPciIntr,

    terminate: bool,

    log: slog::Logger,
}

impl XhciInterrupter {
    pub fn new(number: u16, pci_intr: XhciPciIntr, log: slog::Logger) -> Self {
        let interrupts = Arc::new((
            Mutex::new(InterruptRegulation {
                usbcmd_inte: true,
                number,
                management: bits::InterrupterManagement::default(),
                moderation: bits::InterrupterModeration::default(),
                evt_ring_deq_ptr: bits::EventRingDequeuePointer(0),
                imod_allow_at: Instant::now(),
                imod_timeouts_consecutive: 0,
                intr_pending_enable: false,
                pci_intr,
                terminate: false,
                log,
            }),
            Condvar::new(),
        ));
        let pair = Arc::clone(&interrupts);
        let imod_loop = Some(std::thread::spawn(move || {
            InterruptRegulation::imod_wait_loop(pair)
        }));
        Self {
            number,
            evt_ring_seg_tbl_size: bits::EventRingSegmentTableSize(0),
            evt_ring_seg_base_addr:
                bits::EventRingSegmentTableBaseAddress::default(),
            evt_ring: None,
            interrupts,
            imod_loop,
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
                let duration = regulation
                    .imod_allow_at
                    .saturating_duration_since(Instant::now());
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
                    regulation.imod_allow_at = Instant::now();
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
                if let Some(inst) = Instant::now().checked_add(
                    bits::IMOD_TICK * regulation.moderation.counter() as u32,
                ) {
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
                    self.interrupts.1.notify_one();
                }
                U64(erdp.0)
            }
        };

        let regulation = self.interrupts.0.lock().unwrap();
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
                    event_ring.update_dequeue_pointer(erdp)
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

    pub fn set_pci_intr_mode(&mut self, mode: pci::IntrMode) {
        self.interrupts.0.lock().unwrap().pci_intr.set_mode(mode);
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

impl InterruptRegulation {
    // handling IMODI / IMODC / IP / IE
    // TODO: check if evt ring non-empty as well?
    fn imod_wait_loop(pair: Arc<(Mutex<Self>, Condvar)>) {
        let (regulation, cvar) = &*pair;
        loop {
            let guard = regulation.lock().unwrap();

            if guard.terminate {
                break;
            }

            let timeout = guard
                .imod_allow_at
                .saturating_duration_since(Instant::now())
                .max(guard.moderation.interval() as u32 * bits::IMOD_TICK);

            // xHCI 1.2 figure 4-22
            let (mut guard, timeout_result) = cvar
                .wait_timeout_while(guard, timeout, |ir| {
                    let imod_duration =
                        ir.imod_allow_at.checked_duration_since(Instant::now());
                    !(imod_duration.is_none()
                        && ir.management.enable()
                        && !ir.evt_ring_deq_ptr.handler_busy()
                        && ir.usbcmd_inte
                        && ir.intr_pending_enable)
                })
                .unwrap();
            if !timeout_result.timed_out() {
                guard.imod_timeouts_consecutive = 0;

                if guard.pci_intr.pci_intr_mode == pci::IntrMode::INTxPin
                    && guard.management.pending()
                {
                    let (mut guard, timeout_result) = cvar
                        .wait_timeout_while(
                            guard,
                            Duration::from_secs(5),
                            |ir| {
                                ir.pci_intr.pci_intr_mode
                                    != pci::IntrMode::INTxPin
                                    || !ir.management.pending()
                            },
                        )
                        .unwrap();
                    if timeout_result.timed_out() {
                        slog::error!(
                            guard.log,
                            "INTxPin was already asserted without being cleared by a RW1C write to IP"
                        );
                        // XXX: just deassert it for now?
                        guard.pci_intr.deassert(0);
                        guard.management.set_pending(false);
                    }
                } else {
                    guard.management.set_pending(true);
                    guard.evt_ring_deq_ptr.set_handler_busy(true);
                    guard.pci_intr.fire_interrupt(guard.number);
                    guard.intr_pending_enable = false;
                }
            } else if guard.intr_pending_enable {
                const LIMIT: u32 = 10;
                if guard.imod_timeouts_consecutive < LIMIT {
                    guard.imod_timeouts_consecutive += 1;
                    slog::warn!(
                        guard.log,
                        "IMOD timeout: IMAN enable {}, USBCMD INTE {}",
                        guard.management.enable() as u8,
                        guard.usbcmd_inte as u8,
                    );
                } else if guard.imod_timeouts_consecutive == LIMIT {
                    guard.imod_timeouts_consecutive += 1;
                    slog::warn!(
                        guard.log,
                        "(over {LIMIT} consecutive IMOD timeouts, suppressing further)",
                    );
                }
                if !guard.usbcmd_inte {
                    guard.intr_pending_enable = false;
                }
            }
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
    pub fn set_mode(&mut self, mode: pci::IntrMode) {
        slog::warn!(self.log, "set interrupt mode to {mode:?}");
        self.pci_intr_mode = mode;
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
                        slog::info!(self.log, "xHC interrupter asserting");
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
