// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use crate::common::{GuestAddr, GuestRegion};
use crate::hw::usb::usbdev::demo_state_tracker::NullUsbDevice;
use crate::hw::usb::usbdev::requests::{
    Request, RequestDirection, SetupData, StandardRequest,
};
use crate::hw::usb::xhci::bits::ring_data::{
    Trb, TrbDirection, TrbTransferType, TrbType,
};
use crate::hw::usb::xhci::device_slots::SlotId;
use crate::hw::usb::xhci::rings::consumer::TrbCompletionCode;
use crate::hw::usb::xhci::rings::producer::event::EventInfo;
use crate::vmm::MemCtx;

use super::{ConsumerRing, Error, Result, WorkItem};

#[usdt::provider(provider = "propolis")]
mod probes {
    fn xhci_td_consume(trb_type: u8, size: usize, zero_len: bool) {}
}

pub type TransferRing = ConsumerRing<TransferDescriptor>;

#[derive(Debug)]
pub struct TransferDescriptor {
    pub(super) trbs: Vec<Trb>,
}
impl WorkItem for TransferDescriptor {
    fn try_from_trb_iter(trbs: impl IntoIterator<Item = Trb>) -> Result<Self> {
        let td = Self { trbs: trbs.into_iter().collect() };
        probes::xhci_td_consume!(|| (
            td.trb0_type().map(|t| t as u8).unwrap_or_default(),
            td.transfer_size(),
            td.is_zero_length()
        ));
        Ok(td)
    }
}
impl IntoIterator for TransferDescriptor {
    type Item = Trb;
    type IntoIter = std::vec::IntoIter<Trb>;

    fn into_iter(self) -> Self::IntoIter {
        self.trbs.into_iter()
    }
}

pub enum Never {}
impl TryFrom<Vec<Trb>> for TransferDescriptor {
    type Error = Never;
    fn try_from(trbs: Vec<Trb>) -> core::result::Result<Self, Self::Error> {
        Ok(Self { trbs })
    }
}

#[allow(dead_code)]
impl TransferDescriptor {
    /// xHCI 1.2 sect 4.14: The TD Transfer Size is defined by the sum of the
    /// TRB Transfer Length fields in all TRBs that comprise the TD.
    pub fn transfer_size(&self) -> usize {
        self.trbs
            .iter()
            .map(|trb| unsafe { trb.status.transfer.trb_transfer_length() }
                as usize)
            .sum()
    }

    pub fn trb0_type(&self) -> Option<TrbType> {
        self.trbs.first().map(|trb| trb.control.trb_type())
    }

    /// xHCI 1.2 sect 4.9.1: To generate a zero-length USB transaction,
    /// software shall define a TD with a single Transfer TRB with its
    /// transfer length set to 0. (it may include others, such as Link TRBs or
    /// Event Data TRBs, but only one 'Transfer TRB')
    /// (see also xHCI 1.2 table 6-21; as 4.9.1 is ambiguously worded.
    /// we're looking at *Normal* Transfer TRBs)
    pub fn is_zero_length(&self) -> bool {
        let mut trb_transfer_length = None;
        for trb in &self.trbs {
            if trb.control.trb_type() == TrbType::Normal {
                let x = unsafe { trb.status.transfer.trb_transfer_length() };
                if x != 0 {
                    return false;
                }
                // more than one Normal encountered
                if trb_transfer_length.replace(x).is_some() {
                    return false;
                }
            }
        }
        return true;
    }
}

#[derive(Copy, Clone, Debug)]
pub enum PointerOrImmediate {
    Pointer(GuestRegion),
    #[allow(dead_code)]
    // have not yet implemented anything with out payloads that can use this
    Immediate([u8; 8], usize),
}

impl PointerOrImmediate {
    /// size in bytes.
    pub fn len(&self) -> usize {
        match self {
            PointerOrImmediate::Pointer(GuestRegion(_, len))
            | PointerOrImmediate::Immediate(_, len) => *len,
        }
    }
}

impl From<&Trb> for PointerOrImmediate {
    fn from(trb: &Trb) -> Self {
        let data_length =
            unsafe { trb.status.transfer.trb_transfer_length() as usize };
        // without loss of generality: IDT same bit for all TRB types
        if unsafe { trb.control.normal.immediate_data() } {
            PointerOrImmediate::Immediate(
                trb.parameter.to_ne_bytes(),
                data_length,
            )
        } else {
            PointerOrImmediate::Pointer(GuestRegion(
                GuestAddr(trb.parameter),
                data_length,
            ))
        }
    }
}

#[derive(Debug)]
pub struct TDEventData {
    pub event_data: u64,
    pub interrupt_target_on_completion: Option<u16>,
    pub block_event_interrupt: bool,
    // TODO: Evaluate Next TRB ?
}

impl TryFrom<&Trb> for TDEventData {
    type Error = Error;

    fn try_from(trb: &Trb) -> Result<Self> {
        let trb_type = unsafe { trb.control.status_stage.trb_type() };
        if trb_type != TrbType::EventData {
            Err(Error::WrongTrbType(trb_type, TrbType::EventData))
        } else {
            let interrupt_target_on_completion = unsafe {
                if trb.control.status_stage.interrupt_on_completion() {
                    Some(trb.status.transfer.interrupter_target())
                } else {
                    None
                }
            };
            let block_event_interrupt =
                unsafe { trb.control.normal.block_event_interrupt() };
            Ok(Self {
                event_data: trb.parameter,
                interrupt_target_on_completion,
                block_event_interrupt,
            })
        }
    }
}

#[derive(Debug)]
pub struct TDNormal {
    pub data_buffer: PointerOrImmediate,
    pub interrupt_target_on_completion: Option<u16>,
}

impl TryFrom<&Trb> for TDNormal {
    type Error = Error;

    fn try_from(trb: &Trb) -> Result<Self> {
        let trb_type = unsafe { trb.control.normal.trb_type() };
        if trb_type != TrbType::Normal {
            Err(Error::WrongTrbType(trb_type, TrbType::Normal))
        } else {
            let interrupt_target_on_completion = unsafe {
                if trb.control.normal.interrupt_on_completion() {
                    Some(trb.status.transfer.interrupter_target())
                } else {
                    None
                }
            };
            Ok(Self {
                data_buffer: PointerOrImmediate::from(trb),
                interrupt_target_on_completion,
            })
        }
    }
}

#[derive(Debug)]
pub enum TransferInfo {
    Normal(TDNormal),
    SetupStage {
        data: SetupData,
        interrupt_target_on_completion: Option<u16>,
        transfer_type: TrbTransferType,
    },
    DataStage {
        data_buffer: PointerOrImmediate,
        interrupt_target_on_completion: Option<u16>,
        direction: TrbDirection,
        payload: Vec<TDNormal>,
        event_data: Option<TDEventData>,
    },
    StatusStage {
        interrupt_target_on_completion: Option<u16>,
        direction: TrbDirection,
        event_data: Option<TDEventData>,
    },
    // unimplemented
    Isoch {},
    EventData(TDEventData),
    NoOp,
}

impl TryFrom<TransferDescriptor> for TransferInfo {
    type Error = Error;

    fn try_from(td: TransferDescriptor) -> Result<TransferInfo> {
        let first = td.trbs.first().ok_or(Error::EmptyTransferDescriptor)?;
        let interrupt_target_on_completion = unsafe {
            // without loss of generality (IOC at same bit position in all TRB types)
            if first.control.normal.interrupt_on_completion() {
                Some(first.status.transfer.interrupter_target())
            } else {
                None
            }
        };
        Ok(match first.control.trb_type() {
            TrbType::Normal => TransferInfo::Normal(TDNormal::try_from(first)?),
            TrbType::SetupStage => TransferInfo::SetupStage {
                data: SetupData(first.parameter),
                interrupt_target_on_completion,
                transfer_type: unsafe {
                    first.control.setup_stage.transfer_type()
                },
            },
            TrbType::DataStage => {
                let event_data;
                let payload;
                if unsafe { first.control.data_stage.immediate_data() } {
                    // xHCI 1.2 table 6-29: "If the IDT flag is set in one
                    // Data Stage TRB of a TD, then it shall be the only
                    // Transfer TRB of the TD. An Event Data TRB may also
                    // be included in the TD."
                    payload = Vec::new();
                    event_data = td
                        .trbs
                        .get(1)
                        .map(|trb| TDEventData::try_from(trb))
                        .transpose()?
                } else {
                    // xHCI 1.2 table 6-29 (and sect 3.2.9): "a Data Stage TD is
                    // defined as a Data Stage TRB followed by zero or more
                    // Normal TRBs."
                    //
                    // yes, this seemingly contradicts the IDT description above,
                    // as Event Data TRBs are not Normal TRBs, right?
                    // (more on this later)
                    //
                    // however, xHCI 1.2 sect 4.11.2.2 gives the "rule" that
                    // "A Data Stage TD shall consist of a Data Stage TRB
                    // chained to zero or more Normal TRBs, or Event Data TRBs."
                    //
                    // (to further complicate our terminology:
                    // xHCI table 6-91 defines "Normal" as a TRB Type with ID 1,
                    // and "Event Data" as a TRB Type with ID 7.
                    // xHCI 1.2 sect 1.6 defines "Event Data TRB" as "A Normal
                    // Transfer TRB with its Event Data (ED) flag equal to 1.")
                    payload = td.trbs[1..]
                        .into_iter()
                        .filter(|trb| {
                            trb.control.trb_type() != TrbType::EventData
                        })
                        .map(|trb| TDNormal::try_from(trb))
                        .collect::<Result<Vec<_>>>()?;
                    event_data = td.trbs[1..]
                        .into_iter()
                        .find(|trb| {
                            trb.control.trb_type() == TrbType::EventData
                        })
                        .map(|trb| TDEventData::try_from(trb))
                        .transpose()?;
                };
                TransferInfo::DataStage {
                    data_buffer: PointerOrImmediate::from(first),
                    interrupt_target_on_completion,
                    direction: unsafe { first.control.data_stage.direction() },
                    payload,
                    event_data,
                }
            }
            TrbType::StatusStage => TransferInfo::StatusStage {
                interrupt_target_on_completion,
                direction: unsafe { first.control.status_stage.direction() },
                // xHCI 1.2 table 6-31 (and sect 3.2.9): "a Status Stage TD is
                // defined as a Status Stage TRB followed by zero or one
                // Event Data TRB."
                event_data: td
                    .trbs
                    .get(1)
                    .map(TDEventData::try_from)
                    .transpose()?,
            },
            TrbType::Isoch => TransferInfo::Isoch {},
            TrbType::EventData => {
                TransferInfo::EventData(TDEventData::try_from(first)?)
            }
            TrbType::NoOp => TransferInfo::NoOp,
            _ => return Err(Error::InvalidTransferDescriptor(*first)),
        })
    }
}

pub struct TransferEventParams {
    pub evt_info: EventInfo,
    pub interrupter: u16,
    pub block_event_interrupt: bool,
}

impl TransferInfo {
    #[allow(clippy::too_many_arguments)]
    pub fn run(
        self,
        trb_pointer: GuestAddr,
        slot_id: SlotId,
        endpoint_id: u8,
        evt_data_xfer_len_accum: &mut u32,
        dummy_usbdev_stub: &mut NullUsbDevice,
        memctx: &MemCtx,
        log: &slog::Logger,
    ) -> Vec<TransferEventParams> {
        let mut event_params = self.run_inner(
            trb_pointer,
            slot_id,
            endpoint_id,
            evt_data_xfer_len_accum,
            dummy_usbdev_stub,
            memctx,
            log,
        );
        for TransferEventParams { evt_info, .. } in &mut event_params {
            let EventInfo::Transfer { trb_transfer_length, event_data, .. } =
                evt_info
            else {
                continue;
            };

            if *event_data {
                // xHCI 1.2 sect 4.11.5.2: when Transfer TRB completed,
                // the number of bytes transferred are added to the EDTLA,
                // wrapping at 24-bit max (16,777,215)
                *evt_data_xfer_len_accum &= 0xffffff;
                // xHCI 1.2 table 6-38: if Event Data flag is 1, this field
                // is set to the value of EDTLA
                *trb_transfer_length = *evt_data_xfer_len_accum;
            }
        }
        event_params
    }

    #[allow(clippy::too_many_arguments)]
    fn run_inner(
        self,
        trb_pointer: GuestAddr,
        slot_id: SlotId,
        endpoint_id: u8,
        evt_data_xfer_len_accum: &mut u32,
        dummy_usbdev_stub: &mut NullUsbDevice,
        memctx: &MemCtx,
        log: &slog::Logger,
    ) -> Vec<TransferEventParams> {
        if let TransferInfo::EventData(TDEventData {
            event_data,
            interrupt_target_on_completion,
            block_event_interrupt,
        }) = self
        {
            return interrupt_target_on_completion
                .map(|interrupter| TransferEventParams {
                    evt_info: EventInfo::Transfer {
                        trb_pointer: GuestAddr(event_data),
                        completion_code: TrbCompletionCode::Success,
                        trb_transfer_length: *evt_data_xfer_len_accum,
                        slot_id,
                        endpoint_id,
                        event_data: true,
                    },
                    interrupter,
                    block_event_interrupt,
                })
                .into_iter()
                .collect();
        }

        // xHCI 1.2 sect 4.11.5.2:
        // EDTLA set to 0 prior to executing the first Transfer TRB of a TD
        *evt_data_xfer_len_accum = 0;

        match self {
            TransferInfo::Normal(TDNormal {
                data_buffer,
                interrupt_target_on_completion,
            }) => {
                slog::error!(log, "Normal TD unimplemented (parameter {data_buffer:?}, interrupt target {interrupt_target_on_completion:?})");
                Vec::new()
            }
            TransferInfo::SetupStage {
                data,
                interrupt_target_on_completion,
                transfer_type,
            } => {
                if transfer_type == TrbTransferType::Reserved {
                    slog::error!(
                        log,
                        "usb: Setup Stage TRT reserved ({data:x?})"
                    );
                }
                // xHCI 1.2 sect 4.6.5
                let completion_code = if matches!(
                    data.request(),
                    Request::Standard(StandardRequest::SetAddress)
                ) {
                    slog::error!(log, "attempted to issue a SET_ADDRESS request through a Transfer Ring");
                    TrbCompletionCode::UsbTransactionError
                } else {
                    match dummy_usbdev_stub.setup_stage(data) {
                        Ok(()) => TrbCompletionCode::Success,
                        Err(e) => {
                            slog::error!(log, "USB Setup Stage: {e}");
                            TrbCompletionCode::UsbTransactionError
                        }
                    }
                };
                interrupt_target_on_completion
                    .map(|interrupter| TransferEventParams {
                        evt_info: EventInfo::Transfer {
                            trb_pointer,
                            completion_code,
                            trb_transfer_length: 0,
                            slot_id,
                            endpoint_id,
                            event_data: false,
                        },
                        interrupter,
                        block_event_interrupt: false,
                    })
                    .into_iter()
                    .collect()
            }
            TransferInfo::DataStage {
                data_buffer,
                interrupt_target_on_completion,
                direction,
                payload,
                event_data,
            } => {
                let req_dir = match direction {
                    TrbDirection::Out => RequestDirection::HostToDevice,
                    TrbDirection::In => RequestDirection::DeviceToHost,
                };

                if !payload.is_empty() {
                    slog::warn!(
                        log,
                        "ignoring {} Normal TDs in Data Stage",
                        payload.len(),
                    )
                }

                let (trb_transfer_length, completion_code) =
                    match dummy_usbdev_stub.data_stage(
                        data_buffer,
                        req_dir,
                        &memctx,
                    ) {
                        Ok(x) => (x as u32, TrbCompletionCode::Success),
                        Err(e) => {
                            slog::error!(log, "USB Data Stage: {e}");
                            (0, TrbCompletionCode::UsbTransactionError)
                        }
                    };
                // xHCI 1.2 sect 4.11.5.2: when Transfer TRB completed,
                // the number of bytes transferred are added to the EDTLA
                // (we wrap to 24-bits before using the value elsewhere)
                *evt_data_xfer_len_accum += trb_transfer_length;

                interrupt_target_on_completion
                    .map(|interrupter| TransferEventParams {
                        evt_info: EventInfo::Transfer {
                            trb_pointer,
                            completion_code,
                            trb_transfer_length,
                            slot_id,
                            endpoint_id,
                            event_data: false,
                        },
                        interrupter,
                        block_event_interrupt: false,
                    })
                    .into_iter()
                    .chain(event_data.and_then(
                        |TDEventData {
                             event_data,
                             interrupt_target_on_completion,
                             block_event_interrupt,
                         }| {
                            interrupt_target_on_completion.map(|interrupter| {
                                TransferEventParams {
                                    evt_info: EventInfo::Transfer {
                                        trb_pointer: GuestAddr(event_data),
                                        // xHCI 1.2 sect 4.11.5.2: Event Data
                                        // inherits the completion code of the
                                        // previous TRB
                                        completion_code,
                                        trb_transfer_length: 0,
                                        slot_id,
                                        endpoint_id,
                                        event_data: true,
                                    },
                                    interrupter,
                                    block_event_interrupt,
                                }
                            })
                        },
                    ))
                    .collect()
            }
            TransferInfo::StatusStage {
                interrupt_target_on_completion,
                direction,
                event_data,
            } => {
                let req_dir = match direction {
                    TrbDirection::Out => RequestDirection::HostToDevice,
                    TrbDirection::In => RequestDirection::DeviceToHost,
                };

                let completion_code =
                    match dummy_usbdev_stub.status_stage(req_dir) {
                        Ok(()) => TrbCompletionCode::Success,
                        Err(e) => {
                            slog::error!(log, "USB Status Stage: {e}");
                            TrbCompletionCode::UsbTransactionError
                        }
                    };

                interrupt_target_on_completion
                    .map(|interrupter| TransferEventParams {
                        evt_info: EventInfo::Transfer {
                            trb_pointer,
                            completion_code,
                            trb_transfer_length: 0,
                            slot_id,
                            endpoint_id,
                            event_data: false,
                        },
                        interrupter,
                        block_event_interrupt: false,
                    })
                    .into_iter()
                    .chain(event_data.and_then(
                        |TDEventData {
                             event_data,
                             interrupt_target_on_completion,
                             block_event_interrupt,
                         }| {
                            interrupt_target_on_completion.map(|interrupter| {
                                TransferEventParams {
                                    evt_info: EventInfo::Transfer {
                                        trb_pointer: GuestAddr(event_data),
                                        completion_code,
                                        trb_transfer_length: 0,
                                        slot_id,
                                        endpoint_id,
                                        event_data: true,
                                    },
                                    interrupter,
                                    block_event_interrupt,
                                }
                            })
                        },
                    ))
                    .collect()
            }

            TransferInfo::Isoch {} => {
                // unimplemented on purpose
                slog::warn!(log, "Isochronous TD unimplemented");
                Vec::new()
            }
            TransferInfo::EventData(tdevent_data) => {
                slog::warn!(
                    log,
                    "Event Data TD unimplemented ({tdevent_data:?})"
                );
                Vec::new()
            }
            TransferInfo::NoOp => Vec::new(),
        }
    }
}
