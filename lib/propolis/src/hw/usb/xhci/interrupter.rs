// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

/*! xHC Interrupter, described in xHCI 1.2 section 4.17.

This module contains interfaces to enqueue Event TRBs to the Event Ring and
generate machine interrupts (i.e. MSI-X or INTxPin) when appropriate.

```text
  +-----------------+
  | XhciInterrupter |
  |-----------------|
  | Runtime reg r/w |
  +-----------------+
                     \ +---------------------+
                      \| InterruptRegulation |
   +-------------+     |---------------------|
   | EventSender |---->| EventRing           |
   +-------------+     | XhciPciIntr         |
          ^            | IMODC/IMODI timing  |
          |            +---------------------+
     (Event TRBs)
          |
[[various xHC subsystems]]
```

*/

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, Weak};

use crate::common::{GuestAddr, WriteOp};
use crate::hw::pci;
use crate::hw::usb::xhci::bits;
use crate::hw::usb::xhci::registers::InterrupterRegisters;
use crate::hw::usb::xhci::rings::producer::event::{
    Error as TrbRingProducerError, EventInfo, EventRing,
};
use crate::hw::usb::xhci::{RegRWOpValue, NUM_INTRS};
use crate::vmm::time::{VmGuestInstant, VmGuestTime};
use crate::vmm::MemCtx;

use super::bits::ring_data::TrbCompletionCode;
use super::device_slots::{EndpointId, SlotId};
use super::rings::consumer::transfer::{TransferEventParams, TransferTrb};

#[usdt::provider(provider = "propolis")]
mod probes {
    fn xhci_interrupter_pending(intr_num: u16) {}
    fn xhci_interrupter_fired(intr_num: u16) {}
    fn xhci_pci_interrupt_mode(mode: &str) {}
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("Error in Event Ring operation: {0}")]
    ProducerRing(#[from] TrbRingProducerError),
    #[error(
        "Tried to enqueue Event TRB but EventSender's pci::DeviceState reference was stale"
    )]
    NoPciState,
    #[error(
        "Tried to enqueue Event TRB but xHC's memory access was removed from hierarchy"
    )]
    NoMemAccess,
    #[error("Multiple xHCI interrupters unimplemented")]
    MultipleInterruptersUnimplemented,
    #[error("Tried to access Event Ring before its location was defined by writes to ERSTBA/ERSTSZ")]
    EventRingBeforeRegWrites,
    #[error("Sent Event TRB to stale Event Ring reference")]
    StaleInterrupterReference,
}
pub type Result<T> = core::result::Result<T, Error>;

/// Handles PCI register reads/writes relating to the Interrupter.
pub struct XhciInterrupter {
    /// The number of this interrupter, within 0..[NUM_INTRS]
    number: u16,
    /// ERSTSZ.
    evt_ring_seg_tbl_size: bits::EventRingSegmentTableSize,
    /// ERSTBA.
    evt_ring_seg_base_addr: bits::EventRingSegmentTableBaseAddress,
    /// Data managed by the regulation thread ([XhciInterrupter::imod_loop]).
    interrupts: Arc<(Mutex<InterruptRegulation>, Condvar)>,
    /// Join handle of the regulation thread.
    imod_loop: Option<std::thread::JoinHandle<()>>,
    // used for synchronous chrono-sins.
    guest_time: VmGuestTime,
    pci_state: Weak<pci::DeviceState>,
    log: slog::Logger,
}

/// Manages values relevant to the xHC's rate-limited PCI interrupt needs.
pub struct InterruptRegulation {
    // two out-of-band representatives of xHC-wide registers:
    // ---
    /// A copy of the Interrupter Enable flag in the USBCMD register, set by the
    /// xHC's reg-write handler when written to signal to this interrupter's
    /// worker thread whether the host has enabled system interrupt generation.
    usbcmd_inte: bool,
    /// Pending changes to USBSTS' EINT are stored here until the guest reads
    /// the USBSTS register (at which point it is applied).
    any_ip_raised: Weak<AtomicBool>,

    /// The number of this interrupter, as in [XhciInterrupter::number]
    number: u16,
    /// IMAN register, with Interrupt Enable (IE) and Interrupt Pending (IP).
    management: bits::InterrupterManagement,
    /// IMOD register, the counter (IMODC) and interval (IMODI) used to moderate
    /// the rate of interrupts.
    moderation: bits::InterrupterModeration,

    /// The TRB ring associated with this Interrupter.
    evt_ring: Option<EventRing>,

    /// The Event Ring is lazily instantiated and updated at time of Event TRB
    /// production, as the ERSTBA and ERSTSZ registers may momentarily be
    /// partially-written (as a split 2x 32-bit write) and reading an invalid
    /// segment table through a truncated pointer may generate spurious errors.
    /// (We also cannot know whether a second 32-bit write will ever come --
    /// some systems only write the lower 32-bits in some circumstances,
    /// despite xHCI 1.2 sect 5.1's insistence of how systems should behave
    /// when AC64=1.)
    pending_erstba_erstsz_writes: Option<(GuestAddr, usize)>,

    /// Besides the pointer, the ERDP register also contains the
    /// Event Handler Busy (EHB) flag.
    evt_ring_deq_ptr: bits::EventRingDequeuePointer,

    /// Event Data Transfer Length Accumulator (EDTLA).
    evt_data_transfer_len_accum: u32,

    /// IPE, which acts as a filter to IE to control whether interrupts will be
    /// generated when Event Handler Busy = 0.
    /// See xHCI 1.2 sect 4.17.2. (While you're there, try reading aloud the
    /// phrase "Inversely, inter-interrupt interval value" five times fast.)
    intr_pending_enable: bool,

    /// Simulate IMODC counting up to IMODI by diffing timestamps when read and
    /// sleeping when appropriate.
    imod_allow_at: VmGuestInstant,

    /// Used for firing PCI interrupts
    pci_intr: XhciPciIntr,

    /// Signals the interrupter's worker thread to halt.
    terminate: bool,

    log: slog::Logger,
}

impl XhciInterrupter {
    pub fn new(
        number: u16,
        guest_time: VmGuestTime,
        pci_state: &Arc<pci::DeviceState>,
        any_ip_raised: Weak<AtomicBool>,
        log: slog::Logger,
    ) -> Self {
        let interrupts = Arc::new((
            Mutex::new(InterruptRegulation::new(
                number,
                &guest_time,
                pci_state,
                any_ip_raised,
                &log,
            )),
            Condvar::new(),
        ));
        let pair = Arc::downgrade(&interrupts);
        let guest_time_loop = guest_time.clone();
        let imod_loop = Some(
            std::thread::Builder::new()
                .name(format!("xHCI interrupter {number} IMOD regulation"))
                .spawn(move || {
                    InterruptRegulation::imod_wait_loop(pair, guest_time_loop)
                })
                .unwrap(),
        );
        Self {
            number,
            evt_ring_seg_tbl_size: bits::EventRingSegmentTableSize(0),
            evt_ring_seg_base_addr:
                bits::EventRingSegmentTableBaseAddress::default(),
            interrupts,
            imod_loop,
            guest_time,
            pci_state: Arc::downgrade(pci_state),
            log,
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
                    .saturating_duration_since(self.guest_time.now().unwrap());
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

        let mut notify = false;
        let written_value = match intr_regs {
            InterrupterRegisters::Management => {
                let iman = bits::InterrupterManagement(wo.read_u32());
                let mut regulation = self.interrupts.0.lock().unwrap();
                // RW1C
                if iman.pending() {
                    // deassert pin interrupt on Interrupt Pending clear
                    // (only relevant for INTxPin mode)
                    self.pci_deassert(self.number);
                    // simulate IMODC being loaded with IMODI when IP cleared to 0
                    // (xHCI 1.2 sect 5.5.2.2)
                    regulation.imod_allow_at = self
                        .guest_time
                        .now()
                        .unwrap()
                        .checked_add(regulation.moderation.interval_duration())
                        .unwrap();
                    regulation.management.set_pending(false);
                }
                // RW
                regulation.management.set_enable(iman.enable());
                notify = true;
                // remainder of register is reserved
                U32(iman.0)
            }
            InterrupterRegisters::Moderation => {
                let mut regulation = self.interrupts.0.lock().unwrap();
                regulation.moderation =
                    bits::InterrupterModeration(wo.read_u32());

                // emulating setting the value of IMODC, which counts down to zero.
                if let Some(inst) = self.guest_time.now().unwrap().checked_add(
                    bits::IMOD_TICK * regulation.moderation.counter() as u32,
                ) {
                    regulation.imod_allow_at = inst;
                    notify = true;
                }
                U32(regulation.moderation.0)
            }
            InterrupterRegisters::EventRingSegmentTableSize => {
                self.evt_ring_seg_tbl_size =
                    bits::EventRingSegmentTableSize(wo.read_u32());
                U32(self.evt_ring_seg_tbl_size.0)
            }
            // subject to 64-bit split writes when AC64=1 (xHCI 1.2 sect 5.1)
            // which through our abstraction appear as multiple reads/writes
            // to the same 64-bit register
            InterrupterRegisters::EventRingSegmentTableBaseAddress => {
                self.evt_ring_seg_base_addr.0 = wo.read_u64();
                U64(self.evt_ring_seg_base_addr.0)
            }
            // also subject to 64-bit split writes
            InterrupterRegisters::EventRingDequeuePointer => {
                let erdp = bits::EventRingDequeuePointer(wo.read_u64());
                let mut regulation = self.interrupts.0.lock().unwrap();
                regulation.evt_ring_deq_ptr.set_dequeue_erst_segment_index(
                    erdp.dequeue_erst_segment_index(),
                );
                // RW1C
                if erdp.handler_busy() {
                    regulation.evt_ring_deq_ptr.set_handler_busy(false);
                }
                regulation.evt_ring_deq_ptr.set_pointer(erdp.pointer());
                if let Ok(event_ring) = regulation.event_ring_mut(memctx) {
                    event_ring.update_dequeue_pointer(erdp.pointer());
                    // 4.17.5: "IPE shall be cleared to 0 If the Event Ring transitions to empty"
                    // fig 4-23: "Interrupt Pending Enable is cleared when the Event Ring goes empty"
                    if event_ring.is_empty() {
                        regulation.intr_pending_enable = false;
                    }
                } else if regulation.intr_pending_enable {
                    slog::error!(
                        self.log,
                        "Event Ring absent in ERDP write, after IPE was set"
                    );
                    regulation.intr_pending_enable = false;
                }
                notify = true;
                U64(erdp.0)
            }
        };

        // writes to ERSTBA/ERSTSZ/ERDP themselves don't create the event ring immediately.
        // the first thing that needs the ring creates it based on the value written,
        // and further writes to either half of either register update the event ring
        // the next time it's accessed, via `InterruptRegulation::event_ring_mut`.
        match intr_regs {
            InterrupterRegisters::EventRingSegmentTableSize
            | InterrupterRegisters::EventRingSegmentTableBaseAddress => {
                let erstba = self.evt_ring_seg_base_addr.address();
                let erstsz = self.evt_ring_seg_tbl_size.size() as usize;

                self.interrupts
                    .0
                    .lock()
                    .unwrap()
                    .pending_erstba_erstsz_writes = Some((erstba, erstsz));

                notify = true;
            }
            _ => (),
        }

        if notify {
            self.interrupts.1.notify_one();
        }

        written_value
    }

    // XXX: only call once at PCI dev creation. only want one of these per xhc,
    // but it needs to be able to get at the Mutex<InterruptRegulation>
    // even when it's been swapped out in a reset
    pub(super) fn update_event_sender(&self, sender: &EventSender) {
        *sender.interrupts.lock().unwrap() = Arc::downgrade(&self.interrupts);
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

    pub fn pci_deassert(&self, interrupter_num: u16) {
        // only relevant for INTxPin mode
        if let Some(pin) =
            &self.pci_state.upgrade().and_then(|ps| ps.lintr_pin())
        {
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
}

/// Generates machine interrupts (either MSI-X or INTxPin, as chosen by guest)
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
        if mode != self.pci_intr_mode {
            probes::xhci_pci_interrupt_mode!(|| match mode {
                pci::IntrMode::Disabled => "disabled",
                pci::IntrMode::INTxPin => "INTxPin",
                pci::IntrMode::Msix => "MSI-X",
            });
        }
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
                        slog::trace!(self.log, "xHC interrupter asserting");
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
                    slog::trace!(
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
}

/// An `Arc<EventSender>` is held by the several parts of the xHC that need to
/// produce Event TRBs.
pub struct EventSender {
    // XXX: this is a ridiculous type.
    // its value must be replaced with the new Arc<(Mutex<>, Condvar)>
    // upon host controller reset.
    interrupts: Mutex<Weak<(Mutex<InterruptRegulation>, Condvar)>>,
    pci_state: Weak<pci::DeviceState>,
}

impl EventSender {
    pub fn new(pci_state: &Arc<pci::DeviceState>) -> Self {
        Self {
            interrupts: Mutex::new(Weak::new()),
            pci_state: Arc::downgrade(pci_state),
        }
    }

    /// Enqueue the given [EventInfo] as an Event TRB on this Interrupter's
    /// Event Ring, signaling (via IPE) to generate an interrupt afterward
    /// (unless `block_event_interrupt` is set, naturally).
    /// Returns Ok when an event was enqueued and an interrupt was fired
    pub fn enqueue_event(
        &self,
        event_info: EventInfo,
        block_event_interrupt: bool,
    ) -> Result<()> {
        let pci_state = self.pci_state.upgrade().ok_or(Error::NoPciState)?;
        let memctx = pci_state.acc_mem.access().ok_or(Error::NoMemAccess)?;

        let interrupts = self.interrupts()?;

        let mut regulation = interrupts.0.lock().unwrap();

        let evt_ring = regulation.event_ring_mut(&memctx)?;
        if let Err(e) = evt_ring.enqueue(event_info.into(), &memctx) {
            slog::error!(regulation.log, "failed to enqueue Event TRB: {e}");
            return Err(e.into());
        }
        // check imod/iman for when to fire pci intr
        if !block_event_interrupt {
            regulation.intr_pending_enable = true;
            interrupts.1.notify_one();
            let intr_num = regulation.number;
            probes::xhci_interrupter_pending!(move || intr_num);
        }

        Ok(())
    }

    /// Sets EDTLA=0 when a new Transfer Descriptor is run or the TRDP is set.
    pub fn reset_edtla(&self) {
        if let Ok(interrupts) = self.interrupts() {
            interrupts.0.lock().unwrap().evt_data_transfer_len_accum = 0;
        }
    }

    /// Enqueue a Transfer Event indicating an error (with the given completion
    /// code) occurred during a Transfer Descriptor's processing.
    pub fn send_error_event(
        &self,
        trb_pointer: Option<GuestAddr>,
        completion_code: TrbCompletionCode,
        slot_id: SlotId,
        endpoint_id: EndpointId,
    ) -> Result<()> {
        self.enqueue_event(
            EventInfo::Transfer {
                trb_pointer: trb_pointer.unwrap_or(GuestAddr(0)),
                completion_code,
                trb_transfer_length: 0,
                slot_id,
                endpoint_id,
                event_data: false,
            },
            false,
        )
    }

    /// After a transfer, post the appropriate Event TRBs to the Event Ring
    pub fn send_completion_events_for_trb(
        &self,
        trb: &TransferTrb,
        completion_code: TrbCompletionCode,
        bytes_transferred: usize,
        slot_id: SlotId,
        endpoint_id: EndpointId,
    ) -> Result<()> {
        let interrupter = trb.interrupter_target();
        if interrupter != 0 {
            return Err(Error::MultipleInterruptersUnimplemented);
        }
        // xHCI 1.2 sect 4.11.5.2: when Transfer TRB completed,
        // the number of bytes transferred are added to the EDTLA,
        // wrapping at 24-bit max (16,777,215)
        let edtla = {
            let interrupts = self.interrupts()?;
            let mut guard = interrupts.0.lock().unwrap();
            guard.evt_data_transfer_len_accum += bytes_transferred as u32;
            guard.evt_data_transfer_len_accum &= 0xffffff;
            guard.evt_data_transfer_len_accum
        };

        // The wording in the xHCI spec about this field evidently trips up a lot of devices:
        // https://github.com/torvalds/linux/commit/34b67198244f2d7d8409fa4eb76204c409c0c97e
        let trb_transfer_length = match completion_code {
            // xHCI 1.2 sect 4.10.1.1.2:
            // > If a Short Packet does not occur, then the last TRB of the TD shall generate a
            // > Transfer Event with its Completion Code = Success (assuming there was no
            // > error), its TRB Pointer field pointing to the last Transfer TRB, and the TRB
            // > Transfer Length field shall equal 0.
            TrbCompletionCode::Success => 0,
            // xHCI 1.2 sect 4.10.1, table 6-22:
            // > The Length field of the Transfer Event shall be set to the residual number
            // > of bytes *not* written to the Transfer TRBs’ data buffer.
            //
            // xHCI 1.2 sect 4.10.1.1.2:
            // > TRB Transfer Length field shall indicate the residue bytes *in* the buffer.
            //
            // (both emphases mine) So... is this the right thing to do?
            TrbCompletionCode::ShortPacket => {
                trb.data_buffer().len() - bytes_transferred
            }
            _ => 0,
        } as u32;

        // xHCI 1.2 sect 4.11.3.1:
        // > Transfer Event TRB generation shall only occur under the following
        // > conditions:
        // > - If the Interrupt On Completion (IOC) flag is set.
        // > - When a short transfer occurs during the execution of a Transfer
        // >   TRB and the Interrupt-on-Short Packet (ISP) flag is set.
        // > - If an error occurs during the execution of a Transfer TRB.
        //
        // The latter (error) are handled by [Self::send_error_event].
        let should_interrupt = trb.interrupt_on_completion()
            || (trb.interrupt_on_short_packet()
                && completion_code == TrbCompletionCode::ShortPacket);
        for evt in should_interrupt
            .then_some(TransferEventParams {
                evt_info: EventInfo::Transfer {
                    trb_pointer: trb.trb_pointer(),
                    completion_code,
                    trb_transfer_length,
                    slot_id,
                    endpoint_id,
                    event_data: false,
                },
                interrupter,
                block_event_interrupt: trb.block_event_interrupt(),
            })
            .into_iter()
            .chain(trb.event_data().and_then(|edtrb| {
                // xHCI 1.2 sect 4.11.5.2:
                // > If the TD that incurred the Short Packet is terminated by
                // > an Event Data TRB (with its IOC flag is set), then the xHC
                // > shall generate an Event Data Transfer Event, where the
                // > Length field shall reflect the actual number of bytes
                // > transferred.
                // > The following rules apply to Event Data TRBs [...]:
                // > - An event shall be generated by an Event Data TRB if its
                // >   IOC flag is set to '1'.
                let should_interrupt = edtrb.interrupt_on_completion();
                should_interrupt.then_some(TransferEventParams {
                    evt_info: EventInfo::Transfer {
                        // > The Parameter Component of the Transfer Event
                        // > generated by an Event Data TRB shall contain the
                        // > value of the Event Data TRB Parameter Component.
                        trb_pointer: GuestAddr(edtrb.event_data()),
                        // > The Event Data TRB has the unique properties of
                        // > inheriting the Completion Code of the previous
                        // > (non-Event Data) TRB executed on a ring, and
                        // > accumulating the transfer Lengths of preceding TRBs
                        completion_code,
                        // xHCI 1.2 table 6-38: if Event Data flag is 1, this field
                        // is set to the value of EDTLA.
                        // xHCI 1.2 sect 4.10.1.1.1:
                        // > an Event Data Transfer Event shall be generated with the
                        // > Completion Code set to Short Packet and the Length field
                        // > set to the actual number of bytes received by the TD.
                        trb_transfer_length: edtla,
                        slot_id,
                        endpoint_id,
                        event_data: true,
                    },
                    interrupter,
                    block_event_interrupt: edtrb.block_event_interrupt(),
                })
            }))
        {
            self.enqueue_event(evt.evt_info, evt.block_event_interrupt)?;
        }
        Ok(())
    }

    fn interrupts(&self) -> Result<Arc<(Mutex<InterruptRegulation>, Condvar)>> {
        self.interrupts
            .lock()
            .unwrap()
            .upgrade()
            .ok_or(Error::StaleInterrupterReference)
    }

    #[cfg(test)]
    pub fn set_interrupts(
        &self,
        interrupts: &Arc<(Mutex<InterruptRegulation>, Condvar)>,
    ) {
        *self.interrupts.lock().unwrap() = Arc::downgrade(interrupts);
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

// Excerpt of xHCI 1.2 sect 4.17.5:
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
    fn new(
        number: u16,
        guest_time: &VmGuestTime,
        pci_state: &Arc<pci::DeviceState>,
        any_ip_raised: Weak<AtomicBool>,
        log: &slog::Logger,
    ) -> Self {
        Self {
            usbcmd_inte: true,
            any_ip_raised,
            number,
            management: bits::InterrupterManagement::default(),
            moderation: bits::InterrupterModeration::default(),
            evt_ring: None,
            pending_erstba_erstsz_writes: None,
            evt_ring_deq_ptr: bits::EventRingDequeuePointer(0),
            evt_data_transfer_len_accum: 0,
            imod_allow_at: guest_time.now().unwrap(),
            intr_pending_enable: false,
            pci_intr: XhciPciIntr {
                msix_hdl: pci_state.msix_hdl(),
                pin: pci_state.lintr_pin(),
                pci_intr_mode: pci_state.get_intr_mode(),
                log: log.to_owned(),
            },
            terminate: false,
            log: log.to_owned(),
        }
    }

    #[cfg(test)]
    pub fn new_test(
        event_ring: EventRing,
        pci_state: &Arc<pci::DeviceState>,
        log: &slog::Logger,
    ) -> Self {
        let mut ret =
            Self::new(0, &VmGuestTime::new_test(), pci_state, Weak::new(), log);
        ret.evt_ring = Some(event_ring);
        ret
    }

    /// Access the interrupter's Event Ring, lazily instantiating/updating it
    /// as necessary.
    fn event_ring_mut(&mut self, memctx: &MemCtx) -> Result<&mut EventRing> {
        if self.evt_ring.is_none() {
            let (erstba, erstsz) = self
                .pending_erstba_erstsz_writes
                .ok_or(Error::EventRingBeforeRegWrites)?;
            let erdp = self.evt_ring_deq_ptr.pointer();
            self.evt_ring = Some(EventRing::new(erstba, erstsz, erdp, memctx)?);
            self.pending_erstba_erstsz_writes = None;
        }

        // unwrap: we either set evt_ring to Some() or we returned with ? above
        let event_ring = self.evt_ring.as_mut().unwrap();

        if let Some((erstba, erstsz)) = self.pending_erstba_erstsz_writes.take()
        {
            event_ring.update_segment_table(erstba, erstsz, &memctx)?;
        }

        Ok(event_ring)
    }

    /// Logic for handling IMODI / IMODC / IP / IE and firing interrupts.
    /// See xHCI 1.2 figure 4-22, sect 4.17.2
    fn imod_wait_loop(
        pair: Weak<(Mutex<Self>, Condvar)>,
        guest_time: VmGuestTime,
    ) {
        while let Some(pair) = pair.upgrade() {
            let (regulation, cvar) = &*pair;

            let guard = regulation.lock().unwrap();

            // wait for simulated IMODC to tick down to 0
            let Ok(now) = guest_time.now() else { break };
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
                    !(ir.terminate // was Interrupter dropped (i.e. xHC reset or shutdown)
                        || (
                            // enable system bus interrupt generation (xHCI 1.2 sect 4.2)
                            ir.usbcmd_inte
                            // < Interrupt Enable = '1'? >
                            && ir.management.enable()
                            // < Interrupt Pending Enable? >
                            && ir.intr_pending_enable
                            // < Event Handler *not* Busy? > (EHB = 0)
                            && !ir.evt_ring_deq_ptr.handler_busy()
                        ))
                })
                .unwrap();

            if guard.terminate || guest_time.vmm_destroyed() {
                break;
            }

            if !guard.management.pending() {
                // when any interrupter's IP changes from 0 to 1, set EINT in USBSTS
                let Some(ip_raised) = guard.any_ip_raised.upgrade() else {
                    break;
                };
                ip_raised.store(true, Ordering::Release);
            }

            // [ Interrupt Pending = '1'
            // Event Handler Busy = '1' ]
            guard.management.set_pending(true);
            guard.evt_ring_deq_ptr.set_handler_busy(true);

            // Assertion of IP flag generates an MSI-X/Pin interrupt.
            // IP flag cleared by the completion of PCI write in MSI-X mode,
            // or by software writing the IMAN register in Pin mode.
            // (xHCI 1.2 fig 4-22 description)
            guard.pci_intr.fire_interrupt(guard.number);
            if guard.pci_intr.pci_intr_mode == pci::IntrMode::Msix {
                guard.management.set_pending(false);
            }

            // When IP is asserted, the IMODC is reloaded with the IMODI and
            // the IMODC begins counting down again. (xHCI 1.2 fig 4-23 desc)
            guard.imod_allow_at = guest_time
                .now()
                .unwrap()
                .checked_add(guard.moderation.interval_duration())
                .unwrap();
        }
    }
}

// migration

impl XhciInterrupter {
    pub fn export(
        &self,
    ) -> core::result::Result<
        migrate::XhciInterrupterV1,
        crate::migrate::MigrateStateError,
    > {
        let XhciInterrupter {
            number,
            evt_ring_seg_tbl_size,
            evt_ring_seg_base_addr,
            interrupts,
            imod_loop: _,
            guest_time: _,
            pci_state: _,
            log: _,
        } = self;
        let guard = interrupts.0.lock().unwrap();
        let cvar = &interrupts.1;

        let payload = migrate::XhciInterrupterV1 {
            number: *number,
            evt_ring_seg_tbl_size: evt_ring_seg_tbl_size.0,
            evt_ring_seg_base_addr: evt_ring_seg_base_addr.0,
            usbcmd_inte: guard.usbcmd_inte,
            management: guard.management.0,
            moderation: guard.moderation.0,
            evt_ring: guard.evt_ring.as_ref().map(From::from),
            evt_ring_deq_ptr: guard.evt_ring_deq_ptr.0,
            intr_pending_enable: guard.intr_pending_enable,
            imod_allow_at: guard.imod_allow_at,
            terminate: guard.terminate,
        };
        cvar.notify_one();
        Ok(payload)
    }

    pub fn import(
        &mut self,
        value: migrate::XhciInterrupterV1,
    ) -> core::result::Result<(), crate::migrate::MigrateStateError> {
        let migrate::XhciInterrupterV1 {
            number,
            evt_ring_seg_tbl_size,
            evt_ring_seg_base_addr,
            usbcmd_inte,
            management,
            moderation,
            evt_ring,
            evt_ring_deq_ptr,
            intr_pending_enable,
            imod_allow_at,
            terminate,
        } = value;

        let mut guard = self.interrupts.0.lock().unwrap();
        let cvar = &self.interrupts.1;

        self.number = number;
        self.evt_ring_seg_tbl_size =
            bits::EventRingSegmentTableSize(evt_ring_seg_tbl_size);
        self.evt_ring_seg_base_addr =
            bits::EventRingSegmentTableBaseAddress(evt_ring_seg_base_addr);
        guard.usbcmd_inte = usbcmd_inte;
        guard.number = number;
        guard.management = bits::InterrupterManagement(management);
        guard.moderation = bits::InterrupterModeration(moderation);
        guard.evt_ring = evt_ring.as_ref().map(From::from);
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
    use crate::hw::usb::xhci::rings::producer::event::migrate::*;
    use crate::vmm::time;
    use serde::{Deserialize, Serialize};

    #[derive(Deserialize, Serialize)]
    pub struct XhciInterrupterV1 {
        pub number: u16,
        pub evt_ring_seg_tbl_size: u32,
        pub evt_ring_seg_base_addr: u64,
        pub usbcmd_inte: bool,
        pub management: u32,
        pub moderation: u32,
        pub evt_ring: Option<EventRingV1>,
        pub evt_ring_deq_ptr: u64,
        pub intr_pending_enable: bool,
        pub imod_allow_at: time::VmGuestInstant,
        pub terminate: bool,
    }
}

#[cfg(test)]
mod test {
    use std::time::Duration;

    use super::*;
    use crate::hw::usb::test::pci_state_for_test;
    use crate::hw::usb::xhci::bits::ring_data::{
        EventRingSegment, Trb, TrbType,
    };

    #[test]
    fn event_sender_to_interrupter_event_ring() {
        let log = slog::Logger::root(slog::Discard, slog::o!());
        let pci_state = pci_state_for_test();
        let event_sender = Arc::new(EventSender::new(&pci_state));
        let any_ip_raised = Arc::default();
        let guest_time = VmGuestTime::new_test();
        let memctx = pci_state.acc_mem.access().unwrap();

        let mut intr = XhciInterrupter::new(
            0,
            guest_time,
            &pci_state,
            Arc::downgrade(&any_ip_raised),
            log,
        );
        intr.update_event_sender(&event_sender);

        // explicitly disable throttling (time is fake in unit test)
        intr.reg_write(
            &mut WriteOp::from_buf(
                0,
                &const {
                    bits::InterrupterModeration(0)
                        .with_interval(0)
                        .with_counter(0)
                        .0
                        .to_le_bytes()
                },
            ),
            InterrupterRegisters::Moderation,
            &memctx,
        );

        // haven't yet instantiated Event Ring
        assert!(matches!(
            event_sender.enqueue_event(EventInfo::MfIndexWrap, false),
            Err(Error::EventRingBeforeRegWrites),
        ));

        const ERSTBA: GuestAddr = GuestAddr(0);
        const ERSTSZ: usize = 1;
        const ERST_ENTRIES: [EventRingSegment; 1] = [EventRingSegment {
            base_address: GuestAddr(1024),
            segment_trb_count: 16,
        }];
        const ERDP: GuestAddr = ERST_ENTRIES[0].base_address;

        memctx.write_many(ERSTBA, &ERST_ENTRIES);
        intr.reg_write(
            &mut WriteOp::from_buf(0, &const { ERSTBA.0.to_le_bytes() }),
            InterrupterRegisters::EventRingSegmentTableBaseAddress,
            &memctx,
        );
        intr.reg_write(
            &mut WriteOp::from_buf(0, &const { ERSTSZ.to_le_bytes() }),
            InterrupterRegisters::EventRingSegmentTableSize,
            &memctx,
        );
        intr.reg_write(
            &mut WriteOp::from_buf(0, &const { ERDP.0.to_le_bytes() }),
            InterrupterRegisters::EventRingDequeuePointer,
            &memctx,
        );
        assert!(!intr.interrupts.0.lock().unwrap().intr_pending_enable);
        event_sender.enqueue_event(EventInfo::MfIndexWrap, false).unwrap();
        assert!(intr.interrupts.0.lock().unwrap().intr_pending_enable);

        // the TRB is now in the event ring
        let trb = memctx.read::<Trb>(ERDP).unwrap();
        assert_eq!(
            unsafe { trb.control.event.trb_type() },
            TrbType::MfIndexWrapEvent
        );

        // interrupt won't activate 'til INTE and IMAN are also set
        assert!(!intr.interrupts.0.lock().unwrap().management.pending());
        assert!(!any_ip_raised.load(Ordering::Acquire));

        // INTE enable
        intr.set_usbcmd_inte(true);
        // IMAN enable
        intr.reg_write(
            &mut WriteOp::from_buf(
                0,
                &const {
                    bits::InterrupterManagement(0)
                        .with_enable(true)
                        .0
                        .to_le_bytes()
                },
            ),
            InterrupterRegisters::Management,
            &memctx,
        );
        let (_guard, timeout) = intr
            .interrupts
            .1
            .wait_timeout_while(
                intr.interrupts.0.lock().unwrap(),
                Duration::from_secs(1),
                |x| !x.management.pending(),
            )
            .unwrap();
        assert!(!timeout.timed_out());

        // reflected in USBSTS EINT
        assert!(any_ip_raised.load(Ordering::Acquire));
    }
}
