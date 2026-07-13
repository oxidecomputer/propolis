// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::sync::Mutex;

use crate::common::GuestAddr;
use crate::hw::usb::xhci::controller::XhciState;
use crate::hw::usb::xhci::device_slots::{EndpointId, SlotId};
use crate::hw::usb::xhci::rings::consumer;
use crate::hw::usb::xhci::rings::producer::event::EventInfo;
use crate::vmm::MemCtx;

use super::command::CommandInfo;
use super::transfer::TransferInfo;
use super::TrbCompletionCode;

pub fn command_ring_stop(
    state: &mut XhciState,
    completion_code: TrbCompletionCode,
    log: &slog::Logger,
) {
    state.crcr.set_command_ring_running(false);

    let cmd_trb_addr = state
        .command_ring
        .as_ref()
        .map(|cmd_ring| cmd_ring.current_dequeue_pointer())
        .unwrap_or(GuestAddr(0));
    let event_info = EventInfo::CommandCompletion {
        completion_code,
        slot_id: SlotId::from(0),
        cmd_trb_addr,
    };
    // xHCI 1.2 table 5-24
    if let Err(e) = state.event_sender.enqueue_event(event_info, false) {
        slog::error!(log, "couldn't inform xHCD of stopped Control Ring: {e}");
    } else {
        slog::trace!(log, "stopped Command Ring with {completion_code:?}");
    }
}

/// Called when a doorbell corresponding to an assigned device slot is rung by
/// the guest. Dequeues and executes any available [TransferDescriptor]s from
/// the [TransferRing].
///
/// [TransferDescriptor]: super::transfer::TransferDescriptor
/// [TransferRing]: super::transfer::TransferRing
pub fn process_transfer_ring(
    state: &mut XhciState,
    slot_id: SlotId,
    endpoint_id: EndpointId,
    memctx: &MemCtx,
    log: &slog::Logger,
) {
    loop {
        let raw_td = match state
            .dev_slots
            .transfer_ring_for_doorbell(slot_id, endpoint_id, memctx)
            .map(|xfer_ring| {
                slog::trace!(
                    log,
                    "Transfer Ring at {:#x}",
                    xfer_ring.start_addr.0
                );
                xfer_ring.dequeue_work_item(&memctx)
            }) {
            Ok(raw_td) => raw_td,
            Err(e) => {
                slog::error!(log, "Cannot dequeue from Transfer Ring: {e}");
                break;
            }
        };

        match raw_td.and_then(TransferInfo::try_from) {
            Ok(xfer) => {
                let Ok(usbdev) = state.dev_slots.usbdev_for_slot(slot_id)
                else {
                    slog::error!(log, "No USB device in {slot_id:?}");
                    return;
                };
                let trb_ptr_opt = xfer.first_trb_pointer();
                if let Err(e) = xfer.run(
                    slot_id,
                    endpoint_id,
                    usbdev,
                    &memctx,
                    &state.event_sender,
                    log,
                ) {
                    slog::error!(log, "Error executing Transfer Ring TRB: {e}");
                    if let Some(trb_pointer) = trb_ptr_opt {
                        let evt_info = EventInfo::Transfer {
                            trb_pointer,
                            completion_code:
                                TrbCompletionCode::UsbTransactionError,
                            trb_transfer_length: 0,
                            slot_id,
                            endpoint_id,
                            event_data: false,
                        };
                        if let Err(e) =
                            state.event_sender.enqueue_event(evt_info, false)
                        {
                            slog::error!(log, "Failed to enqueue USB Transaction Error event: {e}");
                        }
                    }
                }
            }
            // Transfer Ring empty
            Err(consumer::Error::EmptyTransferDescriptor) => break,
            Err(consumer::Error::IncompleteWorkItem(trbs)) => {
                // TODO: special-case handling for storing them and completing it
                // (would need adjustment to command trb impls as well)
                slog::warn!(
                    log,
                    "Rewound dequeue pointer after trying to pull \
                    incomplete TD from Transfer Ring: {trbs:?}"
                );
                break;
            }
            Err(e) => {
                slog::error!(log, "Dequeueing TD from endpoint failed: {e}");
                break;
            }
        }
    }
}

/// Called when doorbell 0 is rung by the guest. Dequeues and executes any
/// available [CommandDescriptor]s from the [CommandRing].
///
/// [CommandDescriptor]: super::command::CommandDescriptor
/// [CommandRing]: super::command::CommandRing
pub fn process_command_ring(
    state: &Mutex<XhciState>,
    memctx: &MemCtx,
    log: &slog::Logger,
) {
    loop {
        let mut state = state.lock().unwrap();
        let XhciState {
            event_sender,
            command_ring: Some(cmd_ring),
            crcr,
            dev_slots,
            ..
        } = &mut *state
        else {
            slog::error!(log, "Command Ring not initialized via CRCR");
            break;
        };
        if !crcr.command_ring_running() {
            break;
        }
        match cmd_ring.dequeue_work_item(&memctx) {
            Ok(cmd_desc) => {
                let cmd_trb_addr = cmd_desc.1;
                let cmd = match CommandInfo::try_from(cmd_desc) {
                    Ok(x) => x,
                    Err(e) => {
                        slog::error!(log, "Command Descriptor decoding: {e}");
                        continue;
                    }
                };
                slog::trace!(log, "Command TRB running: {cmd:?}");
                let evt = cmd.run(
                    cmd_trb_addr,
                    dev_slots,
                    memctx,
                    event_sender,
                    &log,
                );
                if let Err(e) = event_sender.enqueue_event(evt, false) {
                    slog::error!(
                        log,
                        "couldn't signal Command TRB completion: {e}"
                    );
                }
            }
            Err(consumer::Error::EmptyCommandDescriptor) => break,
            // matching cycle bits in uninitialized memory trips this.
            Err(consumer::Error::CommandDescriptorSize) => {
                slog::warn!(
                    log,
                    "Command Descriptor appeared to have multiple TRBs, \
                    possible Command Ring memory not yet populated"
                );
                break;
            }
            Err(e) => {
                slog::error!(
                    log,
                    "Failed to dequeue item from Command Ring: {e}"
                );
                break;
            }
        }
    }
}
