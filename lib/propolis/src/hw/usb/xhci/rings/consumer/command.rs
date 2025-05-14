// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use crate::common::GuestAddr;
use crate::hw::usb::xhci::bits::ring_data::{Trb, TrbCompletionCode, TrbType};
use crate::hw::usb::xhci::device_slots::{DeviceSlotTable, SlotId};
use crate::hw::usb::xhci::rings::producer::event::EventInfo;
use crate::hw::usb::xhci::NUM_USB2_PORTS;
use crate::vmm::MemCtx;

use super::{ConsumerRing, Error, Result, WorkItem};

pub type CommandRing = ConsumerRing<CommandDescriptor>;

#[derive(Debug)]
pub struct CommandDescriptor(pub Trb);
impl WorkItem for CommandDescriptor {
    fn try_from_trb_iter(trbs: impl IntoIterator<Item = Trb>) -> Result<Self> {
        let mut trbs = trbs.into_iter();
        if let Some(trb) = trbs.next() {
            if trbs.next().is_some() {
                Err(Error::CommandDescriptorSize)
            } else {
                // xHCI 1.2 sect 6.4.3
                match trb.control.trb_type() {
                    TrbType::NoOpCmd
                    | TrbType::EnableSlotCmd
                    | TrbType::DisableSlotCmd
                    | TrbType::AddressDeviceCmd
                    | TrbType::ConfigureEndpointCmd
                    | TrbType::EvaluateContextCmd
                    | TrbType::ResetEndpointCmd
                    | TrbType::StopEndpointCmd
                    | TrbType::SetTRDequeuePointerCmd
                    | TrbType::ResetDeviceCmd
                    | TrbType::ForceEventCmd
                    | TrbType::NegotiateBandwidthCmd
                    | TrbType::SetLatencyToleranceValueCmd
                    | TrbType::GetPortBandwidthCmd
                    | TrbType::ForceHeaderCmd
                    | TrbType::GetExtendedPropertyCmd
                    | TrbType::SetExtendedPropertyCmd => Ok(Self(trb)),
                    _ => Err(Error::InvalidCommandDescriptor(trb)),
                }
            }
        } else {
            Err(Error::CommandDescriptorSize)
        }
    }
}
impl IntoIterator for CommandDescriptor {
    type Item = Trb;
    type IntoIter = std::iter::Once<Trb>;

    fn into_iter(self) -> Self::IntoIter {
        std::iter::once(self.0)
    }
}

impl TryFrom<CommandDescriptor> for CommandInfo {
    type Error = Error;

    // xHCI 1.2 section 6.4.3
    fn try_from(cmd_desc: CommandDescriptor) -> Result<Self> {
        Ok(match cmd_desc.0.control.trb_type() {
            TrbType::NoOpCmd => CommandInfo::NoOp,
            TrbType::EnableSlotCmd => CommandInfo::EnableSlot {
                slot_type: unsafe { cmd_desc.0.control.slot_cmd.slot_type() },
            },
            TrbType::DisableSlotCmd => CommandInfo::DisableSlot {
                slot_id: unsafe { cmd_desc.0.control.slot_cmd.slot_id() },
            },
            TrbType::AddressDeviceCmd => CommandInfo::AddressDevice {
                input_context_ptr: GuestAddr(cmd_desc.0.parameter & !0b1111),
                slot_id: unsafe { cmd_desc.0.control.slot_cmd.slot_id() },
                block_set_address_request: unsafe {
                    cmd_desc.0.control.slot_cmd.bit9()
                },
            },
            TrbType::ConfigureEndpointCmd => CommandInfo::ConfigureEndpoint {
                input_context_ptr: GuestAddr(cmd_desc.0.parameter & !0b1111),
                slot_id: unsafe { cmd_desc.0.control.slot_cmd.slot_id() },
                deconfigure: unsafe { cmd_desc.0.control.slot_cmd.bit9() },
            },
            TrbType::EvaluateContextCmd => CommandInfo::EvaluateContext {
                input_context_ptr: GuestAddr(cmd_desc.0.parameter & !0b1111),
                slot_id: unsafe { cmd_desc.0.control.slot_cmd.slot_id() },
            },
            TrbType::ResetEndpointCmd => CommandInfo::ResetEndpoint {
                slot_id: unsafe { cmd_desc.0.control.slot_cmd.slot_id() },
                endpoint_id: unsafe {
                    cmd_desc.0.control.endpoint_cmd.endpoint_id()
                },
                transfer_state_preserve: unsafe {
                    cmd_desc.0.control.endpoint_cmd.transfer_state_preserve()
                },
            },
            TrbType::StopEndpointCmd => CommandInfo::StopEndpoint {
                slot_id: unsafe { cmd_desc.0.control.slot_cmd.slot_id() },
                endpoint_id: unsafe {
                    cmd_desc.0.control.endpoint_cmd.endpoint_id()
                },
                suspend: unsafe { cmd_desc.0.control.endpoint_cmd.suspend() },
            },
            TrbType::SetTRDequeuePointerCmd => unsafe {
                CommandInfo::SetTRDequeuePointer {
                    new_tr_dequeue_ptr: GuestAddr(
                        cmd_desc.0.parameter & !0b1111,
                    ),
                    dequeue_cycle_state: (cmd_desc.0.parameter & 1) != 0,
                    // (streams not implemented)
                    // stream_context_type: ((cmd_desc.0.parameter >> 1) & 0b111) as u8,
                    // stream_id: cmd_desc.0.status.command.stream_id(),
                    slot_id: cmd_desc.0.control.endpoint_cmd.slot_id(),
                    endpoint_id: cmd_desc.0.control.endpoint_cmd.endpoint_id(),
                }
            },
            TrbType::ResetDeviceCmd => CommandInfo::ResetDevice {
                slot_id: unsafe { cmd_desc.0.control.slot_cmd.slot_id() },
            },
            // optional normative, ignored by us
            TrbType::ForceEventCmd => CommandInfo::ForceEvent,
            // optional normative, ignored by us
            TrbType::NegotiateBandwidthCmd => CommandInfo::NegotiateBandwidth,
            // optional normative, ignored by us
            TrbType::SetLatencyToleranceValueCmd => {
                CommandInfo::SetLatencyToleranceValue
            }
            // optional
            TrbType::GetPortBandwidthCmd => CommandInfo::GetPortBandwidth {
                port_bandwidth_ctx_ptr: GuestAddr(
                    cmd_desc.0.parameter & !0b1111,
                ),
                hub_slot_id: unsafe {
                    cmd_desc.0.control.get_port_bw_cmd.hub_slot_id()
                },
                dev_speed: unsafe {
                    cmd_desc.0.control.get_port_bw_cmd.dev_speed()
                },
            },
            TrbType::ForceHeaderCmd => CommandInfo::ForceHeader {
                packet_type: (cmd_desc.0.parameter & 0b1_1111) as u8,
                header_info: (cmd_desc.0.parameter >> 5) as u128
                    | ((unsafe { cmd_desc.0.status.command_ext.0 } as u128)
                        << 59),
                root_hub_port_number: unsafe {
                    // hack, same bits
                    cmd_desc.0.control.get_port_bw_cmd.hub_slot_id().into()
                },
            },
            // optional
            TrbType::GetExtendedPropertyCmd => unsafe {
                CommandInfo::GetExtendedProperty {
                    extended_property_ctx_ptr: GuestAddr(
                        cmd_desc.0.parameter & !0b1111,
                    ),
                    extended_capability_id: cmd_desc
                        .0
                        .status
                        .command_ext
                        .extended_capability_id(),
                    command_subtype: cmd_desc.0.control.ext_props_cmd.subtype(),
                    endpoint_id: cmd_desc.0.control.ext_props_cmd.endpoint_id(),
                    slot_id: cmd_desc.0.control.ext_props_cmd.slot_id(),
                }
            },
            // optional
            TrbType::SetExtendedPropertyCmd => unsafe {
                CommandInfo::SetExtendedProperty {
                    extended_capability_id: cmd_desc
                        .0
                        .status
                        .command_ext
                        .extended_capability_id(),
                    capability_parameter: cmd_desc
                        .0
                        .status
                        .command_ext
                        .capability_parameter(),
                    command_subtype: cmd_desc.0.control.ext_props_cmd.subtype(),
                    endpoint_id: cmd_desc.0.control.ext_props_cmd.endpoint_id(),
                    slot_id: cmd_desc.0.control.ext_props_cmd.slot_id(),
                }
            },
            _ => return Err(Error::InvalidCommandDescriptor(cmd_desc.0)),
        })
    }
}

#[derive(Debug)]
pub enum CommandInfo {
    /// xHCI 1.2 sect 3.3.1, 4.6.2
    NoOp,
    /// xHCI 1.2 sect 3.3.1, 4.6.2
    EnableSlot {
        slot_type: u8,
    },
    /// xHCI 1.2 sect 3.3.3, 4.6.4
    DisableSlot {
        slot_id: SlotId,
    },
    /// xHCI 1.2 sect 3.3.4, 4.6.5
    AddressDevice {
        input_context_ptr: GuestAddr,
        slot_id: SlotId,
        block_set_address_request: bool,
    },
    /// xHCI 1.2 sect 3.3.5, 4.6.6
    ConfigureEndpoint {
        input_context_ptr: GuestAddr,
        slot_id: SlotId,
        deconfigure: bool,
    },
    EvaluateContext {
        input_context_ptr: GuestAddr,
        slot_id: SlotId,
    },
    ResetEndpoint {
        slot_id: SlotId,
        endpoint_id: u8,
        transfer_state_preserve: bool,
    },
    StopEndpoint {
        slot_id: SlotId,
        endpoint_id: u8,
        suspend: bool,
    },
    SetTRDequeuePointer {
        new_tr_dequeue_ptr: GuestAddr,
        dequeue_cycle_state: bool,
        slot_id: SlotId,
        endpoint_id: u8,
    },
    ResetDevice {
        slot_id: SlotId,
    },
    ForceEvent,
    NegotiateBandwidth,
    SetLatencyToleranceValue,
    #[allow(unused)]
    GetPortBandwidth {
        port_bandwidth_ctx_ptr: GuestAddr,
        hub_slot_id: SlotId,
        dev_speed: u8,
    },
    /// xHCI 1.2 section 4.6.16
    #[allow(unused)]
    ForceHeader {
        packet_type: u8,
        header_info: u128,
        root_hub_port_number: u8,
    },
    #[allow(unused)]
    GetExtendedProperty {
        extended_property_ctx_ptr: GuestAddr,
        extended_capability_id: u16,
        command_subtype: u8,
        endpoint_id: u8,
        slot_id: SlotId,
    },
    #[allow(unused)]
    SetExtendedProperty {
        extended_capability_id: u16,
        capability_parameter: u8,
        command_subtype: u8,
        endpoint_id: u8,
        slot_id: SlotId,
    },
}

// TODO: return an iterator of EventInfo's for commands that may produce
// multiple Event TRB's, such as the Stop Endpoint Command
impl CommandInfo {
    pub fn run(
        self,
        cmd_trb_addr: GuestAddr,
        dev_slots: &mut DeviceSlotTable,
        memctx: &MemCtx,
    ) -> EventInfo {
        match self {
            // xHCI 1.2 sect 3.3.1, 4.6.2
            CommandInfo::NoOp => EventInfo::CommandCompletion {
                completion_code: TrbCompletionCode::Success,
                slot_id: SlotId::from(0), // 0 for no-op (table 6-42)
                cmd_trb_addr,
            },
            // xHCI 1.2 sect 3.3.2, 4.6.3
            CommandInfo::EnableSlot { slot_type } => {
                match dev_slots.enable_slot(slot_type) {
                    Some(slot_id) => EventInfo::CommandCompletion {
                        completion_code: TrbCompletionCode::Success,
                        slot_id,
                        cmd_trb_addr,
                    },
                    None => EventInfo::CommandCompletion {
                        completion_code:
                            TrbCompletionCode::NoSlotsAvailableError,
                        slot_id: SlotId::from(0),
                        cmd_trb_addr,
                    },
                }
            }
            // xHCI 1.2 sect 3.3.3, 4.6.4
            CommandInfo::DisableSlot { slot_id } => {
                EventInfo::CommandCompletion {
                    completion_code: dev_slots.disable_slot(slot_id, memctx),
                    slot_id,
                    cmd_trb_addr,
                }
            }
            // xHCI 1.2 sect 3.3.4, 4.6.5
            CommandInfo::AddressDevice {
                input_context_ptr,
                slot_id,
                block_set_address_request,
            } => {
                // xHCI 1.2 pg. 113
                let completion_code = dev_slots
                    .address_device(
                        slot_id,
                        input_context_ptr,
                        block_set_address_request,
                        memctx,
                    )
                    // we'll call invalid pointers a context state error
                    .unwrap_or(TrbCompletionCode::ContextStateError);

                EventInfo::CommandCompletion {
                    completion_code,
                    slot_id,
                    cmd_trb_addr,
                }
            }
            // xHCI 1.2 sect 3.3.5, 4.6.6
            CommandInfo::ConfigureEndpoint {
                input_context_ptr,
                slot_id,
                deconfigure,
            } => {
                let completion_code = dev_slots
                    .configure_endpoint(
                        input_context_ptr,
                        slot_id,
                        deconfigure,
                        memctx,
                    )
                    .unwrap_or(TrbCompletionCode::ResourceError);
                EventInfo::CommandCompletion {
                    completion_code,
                    slot_id,
                    cmd_trb_addr,
                }
            }
            CommandInfo::EvaluateContext { input_context_ptr, slot_id } => {
                let completion_code = dev_slots
                    .evaluate_context(slot_id, input_context_ptr, memctx)
                    // TODO: handle properly. for now just:
                    .unwrap_or(TrbCompletionCode::ContextStateError);
                EventInfo::CommandCompletion {
                    completion_code,
                    slot_id,
                    cmd_trb_addr,
                }
            }
            CommandInfo::ResetEndpoint {
                slot_id,
                endpoint_id,
                transfer_state_preserve,
            } => {
                let completion_code = dev_slots
                    .reset_endpoint(
                        slot_id,
                        endpoint_id,
                        transfer_state_preserve,
                        memctx,
                    )
                    .unwrap_or(TrbCompletionCode::ContextStateError);
                EventInfo::CommandCompletion {
                    completion_code,
                    slot_id,
                    cmd_trb_addr,
                }
            }
            CommandInfo::StopEndpoint { slot_id, endpoint_id, suspend } => {
                let completion_code = dev_slots
                    .stop_endpoint(slot_id, endpoint_id, suspend, memctx)
                    .unwrap_or(TrbCompletionCode::ContextStateError);
                EventInfo::CommandCompletion {
                    completion_code,
                    slot_id,
                    cmd_trb_addr,
                }
            }
            CommandInfo::SetTRDequeuePointer {
                new_tr_dequeue_ptr,
                dequeue_cycle_state,
                slot_id,
                endpoint_id,
            } => {
                let completion_code = dev_slots
                    .set_tr_dequeue_pointer(
                        new_tr_dequeue_ptr,
                        slot_id,
                        endpoint_id,
                        dequeue_cycle_state,
                        memctx,
                    )
                    .unwrap_or(TrbCompletionCode::ContextStateError);
                EventInfo::CommandCompletion {
                    completion_code,
                    slot_id,
                    cmd_trb_addr,
                }
            }
            CommandInfo::ResetDevice { slot_id } => {
                let completion_code = dev_slots
                    .reset_device(slot_id, memctx)
                    .unwrap_or(TrbCompletionCode::ContextStateError);
                EventInfo::CommandCompletion {
                    completion_code,
                    slot_id,
                    cmd_trb_addr,
                }
            }
            // xHCI 1.2 section 4.6.16
            CommandInfo::ForceHeader {
                packet_type: _,
                header_info: _,
                root_hub_port_number,
            } => {
                let completion_code = match root_hub_port_number {
                    0..NUM_USB2_PORTS => {
                        // TODO: transmit Force Header packet
                        TrbCompletionCode::UndefinedError
                    }
                    _ => TrbCompletionCode::TrbError,
                };
                EventInfo::CommandCompletion {
                    completion_code,
                    slot_id: SlotId::from(0),
                    cmd_trb_addr,
                }
            }
            // optional, unimplemented
            CommandInfo::ForceEvent
            | CommandInfo::NegotiateBandwidth
            | CommandInfo::SetLatencyToleranceValue => {
                EventInfo::CommandCompletion {
                    completion_code: TrbCompletionCode::TrbError,
                    slot_id: SlotId::from(0),
                    cmd_trb_addr,
                }
            }
            // optional, unimplemented
            CommandInfo::GetPortBandwidth { hub_slot_id: slot_id, .. }
            | CommandInfo::GetExtendedProperty { slot_id, .. }
            | CommandInfo::SetExtendedProperty { slot_id, .. } => {
                EventInfo::CommandCompletion {
                    completion_code: TrbCompletionCode::TrbError,
                    slot_id,
                    cmd_trb_addr,
                }
            }
        }
    }
}
