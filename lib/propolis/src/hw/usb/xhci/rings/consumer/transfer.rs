// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! This module concerns decoding and dispatching Transfer TRBs.
//! (xHCI 1.2 sect 6.4.1)
//!
//! The [TransferRing] is a consumer ring that constructs [TransferDescriptor]s
//! from continuous chains of consumed TRBs, which are in turn validated and
//! converted into instances of the [TransferInfo] enum for proper handling.
//!
//! [TransferInfo::run] is responsible for calling the appropriate [UsbDevice]
//! function for the transfer it represents.
//!
//! [UsbDevice]: crate::hw::usb::usbdev::UsbDevice

use crate::common::{GuestAddr, GuestRegion};
use crate::hw::usb::usbdev::requests::{
    RequestDirection, RequestType, SetupData, StandardRequest,
};
use crate::hw::usb::usbdev::UsbDevice;
use crate::hw::usb::xhci::bits::ring_data::{
    Trb, TrbDirection, TrbTransferType, TrbType,
};
use crate::hw::usb::xhci::device_slots::{EndpointId, SlotId};
use crate::hw::usb::xhci::interrupter::EventSender;
use crate::hw::usb::xhci::rings::consumer::TrbCompletionCode;
use crate::hw::usb::xhci::rings::producer::event::EventInfo;
use crate::hw::usb::xhci::NUM_INTRS;
use crate::vmm::MemCtx;

use super::{ConsumerRing, Error, Result, WorkItem};

#[usdt::provider(provider = "propolis")]
mod probes {
    fn xhci_td_consume(trb_type: u8, size: usize, zero_len: bool) {}
}

pub type TransferRing = ConsumerRing<TransferDescriptor>;

#[derive(Debug)]
pub struct TransferDescriptor {
    pub(super) trbs: Vec<(Trb, GuestAddr)>,
}
impl WorkItem for TransferDescriptor {
    fn try_from_trb_iter(
        trbs: impl IntoIterator<Item = (Trb, GuestAddr)>,
    ) -> Result<Self> {
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
    type Item = (Trb, GuestAddr);
    type IntoIter = std::vec::IntoIter<Self::Item>;

    fn into_iter(self) -> Self::IntoIter {
        self.trbs.into_iter()
    }
}

impl From<Vec<(Trb, GuestAddr)>> for TransferDescriptor {
    fn from(trbs: Vec<(Trb, GuestAddr)>) -> Self {
        Self { trbs }
    }
}

#[allow(dead_code)]
impl TransferDescriptor {
    /// xHCI 1.2 sect 4.14: The TD Transfer Size is defined by the sum of the
    /// TRB Transfer Length fields in all TRBs that comprise the TD.
    pub fn transfer_size(&self) -> usize {
        self.trbs
            .iter()
            .map(
                |(trb, _)| unsafe { trb.status.transfer.trb_transfer_length() }
                    as usize,
            )
            .sum()
    }

    pub fn trb0_type(&self) -> Option<TrbType> {
        self.trbs.first().map(|(trb, _)| trb.control.trb_type())
    }

    /// xHCI 1.2 sect 4.9.1: To generate a zero-length USB transaction,
    /// software shall define a TD with a single Transfer TRB with its
    /// transfer length set to 0. (it may include others, such as Link TRBs or
    /// Event Data TRBs, but only one 'Transfer TRB')
    /// (see also xHCI 1.2 table 6-21; as 4.9.1 is ambiguously worded.
    /// we're looking at *Normal* Transfer TRBs)
    pub fn is_zero_length(&self) -> bool {
        let mut trb_transfer_length = None;
        for (trb, _) in &self.trbs {
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
    fn to_transfer_trbs(&self) -> Result<Vec<TransferTrb>> {
        let mut xfer_trbs = Vec::with_capacity(self.trbs.len());
        let mut iter = self.trbs.iter().peekable();
        let mut using_ioc = false;
        let mut using_edtrb = false;
        while let Some((trb, addr)) = iter.next() {
            using_ioc |= unsafe {
                trb.control.normal.interrupt_on_completion()
                    || trb.control.normal.interrupt_on_short_packet()
            };
            let edtrb = iter
                .next_if(|(edtrb, _)| {
                    edtrb.control.trb_type() == TrbType::EventData
                })
                // unwrap: only error in TryFrom impl is type not being EventData
                .map(|(edtrb, _)| EventDataTrb::try_from(edtrb).unwrap());
            using_edtrb |= edtrb.is_some();
            if using_ioc && using_edtrb {
                // per EDTRB rules from xHCI 1.2 sect 4.11.7:
                // """
                // If Event Data TRBs are defined within a TD, then the IOC or ISP flags shall not be
                // set in any Transfer TRB of a TD. i.e. the use of Event Data Transfer Events and
                // normal Transfer Events to report a TD completion are mutually exclusive.
                // """
                return Err(Error::TDWithBothInterruptSchemes);
            }
            xfer_trbs.push(TransferTrb::new(trb, addr, edtrb)?);
        }
        Ok(xfer_trbs)
    }
}

#[derive(Copy, Clone, Debug)]
pub enum PointerOrImmediate {
    Pointer(GuestRegion),
    #[allow(dead_code)]
    // have not yet implemented anything with Out payloads that can use this
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

#[derive(Debug, Copy, Clone)]
pub struct EventDataTrb {
    trb: Trb,
}

impl TryFrom<&Trb> for EventDataTrb {
    type Error = Error;

    fn try_from(trb: &Trb) -> Result<Self> {
        let trb_type = unsafe { trb.control.status_stage.trb_type() };
        if trb_type != TrbType::EventData {
            Err(Error::WrongTrbType(trb_type, TrbType::EventData))
        } else {
            Ok(Self { trb: *trb })
        }
    }
}

impl EventDataTrb {
    pub fn event_data(&self) -> u64 {
        self.trb.parameter
    }
    pub fn interrupter_target(&self) -> u16 {
        unsafe { self.trb.status.transfer }.interrupter_target()
    }
    pub fn interrupt_on_completion(&self) -> bool {
        unsafe { self.trb.control.normal }.interrupt_on_completion()
    }
    pub fn interrupt_on_short_packet(&self) -> bool {
        unsafe { self.trb.control.normal }.interrupt_on_short_packet()
    }
    pub fn block_event_interrupt(&self) -> bool {
        unsafe { self.trb.control.normal }.block_event_interrupt()
    }
}

// WIP: test that we can serialize scatter-gather properly in the future,
// i.e. multiple normal TRBs and event data TRBs
#[derive(Debug, Copy, Clone)]
pub struct TransferTrb {
    trb: Trb,
    addr: GuestAddr,
    event_data: Option<EventDataTrb>,
}

impl TransferTrb {
    pub fn data_buffer(&self) -> PointerOrImmediate {
        PointerOrImmediate::from(&self.trb)
    }
    pub fn interrupter_target(&self) -> u16 {
        unsafe { self.trb.status.transfer }.interrupter_target()
    }
    pub fn interrupt_on_completion(&self) -> bool {
        unsafe { self.trb.control.normal }.interrupt_on_completion()
    }
    pub fn interrupt_on_short_packet(&self) -> bool {
        unsafe { self.trb.control.normal }.interrupt_on_short_packet()
    }
    pub fn block_event_interrupt(&self) -> bool {
        unsafe { self.trb.control.normal }.block_event_interrupt()
    }

    pub fn trb_pointer(&self) -> GuestAddr {
        self.addr
    }
    pub fn cycle_state(&self) -> bool {
        self.trb.control.cycle()
    }
    pub fn event_data(&self) -> Option<&EventDataTrb> {
        self.event_data.as_ref()
    }

    pub fn new(
        trb: &Trb,
        ptr: &GuestAddr,
        event_data: Option<EventDataTrb>,
    ) -> Result<Self> {
        let trb_type = unsafe { trb.control.normal.trb_type() };
        if matches!(trb_type, TrbType::Normal | TrbType::DataStage) {
            Ok(Self { trb: *trb, addr: *ptr, event_data })
        } else {
            Err(Error::WrongTrbType(trb_type, TrbType::Normal))
        }
    }
}

#[derive(Debug)]
pub enum TransferInfo {
    Normal(Vec<TransferTrb>),
    SetupStage {
        data: SetupData,
        interrupt_target_on_completion: Option<u16>,
        transfer_type: TrbTransferType,
        trb_pointer: GuestAddr,
    },
    DataStage {
        direction: TrbDirection,
        transfer_trbs: Vec<TransferTrb>,
    },
    StatusStage {
        interrupt_target_on_completion: Option<u16>,
        direction: TrbDirection,
        event_data: Option<EventDataTrb>,
        trb_pointer: GuestAddr,
    },
    // unimplemented
    Isoch {},
    // xHCI 1.2 sect 4.11.7:
    // """
    // Software may insert an Event Data TD immediately following a TD to
    // provide additional information related to the previous TD. An Event
    // Data TD is a TD that consists of just one Event Data TRB.
    // """
    EventData(EventDataTrb),
    NoOp,
}

impl TransferInfo {
    pub fn first_trb_pointer(&self) -> Option<GuestAddr> {
        match self {
            Self::Normal(transfer_trbs)
            | Self::DataStage { transfer_trbs, .. } => {
                transfer_trbs.first().map(|trb| trb.trb_pointer())
            }
            Self::SetupStage { trb_pointer, .. }
            | Self::StatusStage { trb_pointer, .. } => Some(*trb_pointer),
            _ => None,
        }
    }
}

impl TryFrom<TransferDescriptor> for TransferInfo {
    type Error = Error;

    fn try_from(td: TransferDescriptor) -> Result<TransferInfo> {
        let (first, ptr) =
            td.trbs.first().ok_or(Error::EmptyTransferDescriptor)?;
        let interrupt_target_on_completion = unsafe {
            // without loss of generality (IOC at same bit position in all TRB types)
            if first.control.normal.interrupt_on_completion() {
                Some(first.status.transfer.interrupter_target())
            } else {
                None
            }
        };
        Ok(match first.control.trb_type() {
            TrbType::Normal => TransferInfo::Normal(td.to_transfer_trbs()?),
            TrbType::SetupStage => TransferInfo::SetupStage {
                data: SetupData(first.parameter),
                interrupt_target_on_completion,
                transfer_type: unsafe {
                    first.control.setup_stage.transfer_type()
                },
                trb_pointer: *ptr,
            },
            TrbType::DataStage => {
                if unsafe { first.control.data_stage.immediate_data() } {
                    // xHCI 1.2 table 6-29: "If the IDT flag is set in one
                    // Data Stage TRB of a TD, then it shall be the only
                    // Transfer TRB of the TD. An Event Data TRB may also
                    // be included in the TD."
                    let event_data = td
                        .trbs
                        .get(1)
                        .map(|(trb, _)| EventDataTrb::try_from(trb))
                        .transpose()?;
                    TransferInfo::DataStage {
                        direction: unsafe {
                            first.control.data_stage.direction()
                        },
                        transfer_trbs: vec![TransferTrb::new(
                            first, ptr, event_data,
                        )?],
                    }
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
                    TransferInfo::DataStage {
                        direction: unsafe {
                            first.control.data_stage.direction()
                        },
                        transfer_trbs: td.to_transfer_trbs()?,
                    }
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
                    .map(|(trb, _)| EventDataTrb::try_from(trb))
                    .transpose()?,
                trb_pointer: *ptr,
            },
            TrbType::Isoch => TransferInfo::Isoch {},
            TrbType::EventData => {
                TransferInfo::EventData(EventDataTrb::try_from(first)?)
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
    /// This method enqueues completion events for successfully executed TRBs.
    ///
    /// *Caller* must put a USB Transaction Error event into the Event Ring
    /// when Err(_) is returned.
    pub fn run(
        self,
        slot_id: SlotId,
        endpoint_id: EndpointId,
        usbdev: &mut Box<dyn UsbDevice>,
        memctx: &MemCtx,
        event_sender: &EventSender,
        log: &slog::Logger,
    ) -> Result<()> {
        // NOTE: EventSender only supports one interrupter, so interrupter fields are ignored
        const {
            assert!(NUM_INTRS == 1);
        }

        let enqueue_success =
            |trb_pointer, event_data, block_event_interrupt| {
                if let Err(e) = event_sender.enqueue_event(
                    EventInfo::Transfer {
                        trb_pointer,
                        completion_code: TrbCompletionCode::Success,
                        trb_transfer_length: 0,
                        slot_id,
                        endpoint_id,
                        event_data,
                    },
                    block_event_interrupt,
                ) {
                    slog::error!(
                        log,
                        "error enqueueing Transfer TRB completion event: {e}"
                    )
                }
            };

        // xHCI 1.2 sect 4.11.5.2:
        // EDTLA set to 0 prior to executing the first Transfer TRB of a TD
        usbdev.new_transfer_descriptor();

        match self {
            TransferInfo::Normal(xfer_trbs) => {
                // responsible for enqueueing its own successful completion events
                usbdev.normal_transfer(endpoint_id, xfer_trbs)?;
            }
            TransferInfo::SetupStage {
                data,
                interrupt_target_on_completion,
                transfer_type,
                trb_pointer,
            } => {
                if transfer_type == TrbTransferType::Reserved {
                    slog::error!(
                        log,
                        "usb: Setup Stage TRT reserved ({data:x?})"
                    );
                }
                // xHCI 1.2 sect 4.6.5
                if matches!(
                    (
                        data.request_type(),
                        StandardRequest::from_repr(data.request())
                    ),
                    (RequestType::Standard, Some(StandardRequest::SetAddress))
                ) {
                    return Err(Error::SetAddressViaTRB(trb_pointer));
                }

                usbdev.setup_stage(endpoint_id, data)?;

                if let Some(_interrupter) = interrupt_target_on_completion {
                    enqueue_success(trb_pointer, false, false);
                }
            }
            TransferInfo::DataStage { direction, transfer_trbs } => {
                let req_dir = match direction {
                    TrbDirection::Out => RequestDirection::HostToDevice,
                    TrbDirection::In => RequestDirection::DeviceToHost,
                };

                // responsible for enqueueing its own successful completion events
                usbdev.data_stage(
                    endpoint_id,
                    transfer_trbs,
                    req_dir,
                    memctx,
                )?;
            }
            TransferInfo::StatusStage {
                interrupt_target_on_completion,
                direction,
                event_data,
                trb_pointer,
            } => {
                let req_dir = match direction {
                    TrbDirection::Out => RequestDirection::HostToDevice,
                    TrbDirection::In => RequestDirection::DeviceToHost,
                };

                usbdev.status_stage(endpoint_id, req_dir)?;

                if let Some(_interrupter) = interrupt_target_on_completion {
                    enqueue_success(trb_pointer, false, false);
                }
                if let Some(ed) = event_data {
                    if ed.interrupt_on_completion() {
                        enqueue_success(
                            GuestAddr(ed.event_data()),
                            true,
                            ed.block_event_interrupt(),
                        );
                    }
                }
            }

            TransferInfo::Isoch {} => {
                // unimplemented on purpose
                slog::warn!(log, "Isochronous TD unimplemented");
            }
            TransferInfo::EventData(ed) => {
                if ed.interrupt_on_completion() {
                    enqueue_success(
                        GuestAddr(ed.event_data()),
                        true,
                        ed.block_event_interrupt(),
                    );
                }
            }
            TransferInfo::NoOp => {}
        }
        Ok(())
    }
}

pub mod migrate {
    use serde::{Deserialize, Serialize};

    use crate::{
        common::GuestAddr,
        hw::usb::xhci::bits::ring_data::{
            Trb, TrbControlField, TrbControlFieldNormal, TrbStatusField,
            TrbStatusFieldTransfer,
        },
    };

    use super::{EventDataTrb, TransferTrb};

    #[derive(Serialize, Deserialize)]
    pub struct TrbV1 {
        pub parameter: u64,
        pub status: u32,
        pub control: u32,
    }

    impl From<&TrbV1> for Trb {
        fn from(value: &TrbV1) -> Self {
            let TrbV1 { parameter, status, control } = value;
            Self {
                parameter: *parameter,
                status: TrbStatusField {
                    transfer: TrbStatusFieldTransfer(*status),
                },
                control: TrbControlField {
                    normal: TrbControlFieldNormal(*control),
                },
            }
        }
    }

    impl From<&Trb> for TrbV1 {
        fn from(trb: &Trb) -> Self {
            Self {
                parameter: trb.parameter,
                status: unsafe { trb.status.transfer.0 },
                control: unsafe { trb.control.normal.0 },
            }
        }
    }

    #[derive(Serialize, Deserialize)]
    pub struct TransferTrbV1 {
        pub trb: TrbV1,
        pub trb_addr: u64,
        pub event_data: Option<EventDataTrbV1>,
    }

    impl From<&TransferTrbV1> for TransferTrb {
        fn from(value: &TransferTrbV1) -> Self {
            let TransferTrbV1 { trb, trb_addr, event_data } = value;
            Self {
                trb: Trb::from(trb),
                addr: GuestAddr(*trb_addr),
                event_data: event_data.as_ref().map(From::from),
            }
        }
    }

    impl From<&TransferTrb> for TransferTrbV1 {
        fn from(value: &TransferTrb) -> Self {
            let TransferTrb { trb: raw_trb, addr: trb_addr, event_data } =
                value;
            Self {
                trb: TrbV1::from(raw_trb),
                trb_addr: trb_addr.0,
                event_data: event_data.as_ref().map(From::from),
            }
        }
    }

    #[derive(Serialize, Deserialize)]
    pub struct EventDataTrbV1 {
        trb: TrbV1,
    }

    impl From<&EventDataTrbV1> for EventDataTrb {
        fn from(value: &EventDataTrbV1) -> Self {
            Self { trb: Trb::from(&value.trb) }
        }
    }

    impl From<&EventDataTrb> for EventDataTrbV1 {
        fn from(value: &EventDataTrb) -> Self {
            Self { trb: TrbV1::from(&value.trb) }
        }
    }
}
