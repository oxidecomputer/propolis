// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use crate::{common::GuestAddr, hw::usb::xhci::device_slots::SlotId};
use bitstruct::bitstruct;
use strum::FromRepr;
use zerocopy::{FromBytes, Immutable};

/// xHCI 1.2 sect 6.5
#[repr(C)]
#[derive(Copy, Clone, Debug, FromBytes, Immutable)]
pub struct EventRingSegment {
    /// Ring Segment Base Address. Lower 6 bits are reserved (addresses are 64-byte aligned).
    pub base_address: GuestAddr,
    /// Ring Segment Size. Valid values are between 16 and 4096.
    pub segment_trb_count: usize,
}

#[repr(C)]
#[derive(Copy, Clone, FromBytes, Immutable)]
pub struct Trb {
    /// may be an address or immediate data
    pub parameter: u64,
    pub status: TrbStatusField,
    pub control: TrbControlField,
}

impl core::fmt::Debug for Trb {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Trb {{ parameter: 0x{:x}, control.trb_type: {:?} }}",
            self.parameter,
            self.control.trb_type()
        )?;
        Ok(())
    }
}

impl Default for Trb {
    fn default() -> Self {
        Self {
            parameter: 0,
            status: Default::default(),
            control: TrbControlField { normal: Default::default() },
        }
    }
}

/// Representations of the 'control' field of Transfer Request Block (TRB).
/// The field definitions differ depending on the TrbType.
/// See xHCI 1.2 Section 6.4.1 (Comments are paraphrases thereof)
#[derive(Copy, Clone, FromBytes, Immutable)]
pub union TrbControlField {
    pub normal: TrbControlFieldNormal,
    pub setup_stage: TrbControlFieldSetupStage,
    pub data_stage: TrbControlFieldDataStage,
    pub status_stage: TrbControlFieldStatusStage,
    pub link: TrbControlFieldLink,
    pub event: TrbControlFieldEvent,
    pub transfer_event: TrbControlFieldTransferEvent,
    pub slot_cmd: TrbControlFieldSlotCmd,
    pub endpoint_cmd: TrbControlFieldEndpointCmd,
    pub get_port_bw_cmd: TrbControlFieldGetPortBandwidthCmd,
    pub ext_props_cmd: TrbControlFieldExtendedPropsCmd,
}

impl TrbControlField {
    pub fn trb_type(&self) -> TrbType {
        // all variants are alike in TRB type location
        unsafe { self.normal.trb_type() }
    }

    pub fn cycle(&self) -> bool {
        // all variants are alike in cycle bit location
        unsafe { self.normal.cycle() }
    }

    pub fn set_cycle(&mut self, cycle_state: bool) {
        // all variants are alike in cycle bit location
        unsafe { self.normal.set_cycle(cycle_state) }
    }

    pub fn chain_bit(&self) -> Option<bool> {
        Some(match self.trb_type() {
            TrbType::Normal => unsafe { self.normal.chain_bit() },
            TrbType::DataStage => unsafe { self.data_stage.chain_bit() },
            TrbType::StatusStage => unsafe { self.status_stage.chain_bit() },
            TrbType::Link => unsafe { self.link.chain_bit() },
            _ => return None,
        })
    }
}

bitstruct! {
    /// Normal TRB control fields (xHCI 1.2 table 6-22)
    #[derive(Clone, Copy, Debug, Default, FromBytes, Immutable)]
    pub struct TrbControlFieldNormal(pub u32) {
        /// Used to mark the Enqueue Pointer of the Transfer Ring.
        pub cycle: bool = 0;

        /// Or "ENT". If set, the xHC shall fetch and evaluate the next TRB
        /// before saving the endpoint state (see xHCI 1.2 Section 4.12.3)
        pub evaluate_next_trb: bool = 1;

        /// Or "ISP". If set, and a Short Packet is encountered for this TRB
        /// (less than the amount specified in the TRB Transfer Length),
        /// then a Transfer Event TRB shall be generated with its
        /// Completion Code set to Short Packet and its TRB Transfer Length
        /// field set to the residual number of bytes not transfered into
        /// the associated data buffer.
        pub interrupt_on_short_packet: bool = 2;

        /// Or "NS". If set, xHC is permitted to set the No Snoop bit in the
        /// Requester attributes of the PCIe transactions it initiates if the
        /// PCIe Enable No Snoop flag is also set. (see xHCI 1.2 sect 4.18.1)
        pub no_snoop: bool = 3;

        /// Or "CH". If set, this TRB is associated with the next TRB on the
        /// ring. The last TRB of a Transfer Descriptor is always unset (0).
        pub chain_bit: bool = 4;

        /// Or "IOC". If set, when this TRB completes, the xHC shall notify
        /// the system of completion by enqueueing a Transfer Event TRB on the
        /// Event ring and triggering an interrupt as appropriate.
        /// (see xHCI 1.2 sect 4.10.4, 4.17.5)
        pub interrupt_on_completion: bool = 5;

        /// Or "IDT". If set, the Data Buffer Pointer field ([Trb::parameter])
        /// is not a pointer, but an array of between 0 and 8 bytes (specified
        /// by the TRB Transfer Length field). Never set on IN endpoints or
        /// endpoints that define a Max Packet Size less than 8 bytes.
        pub immediate_data: bool = 6;

        reserved1: u8 = 7..9;

        /// Or "BEI". If this and `interrupt_on_completion` are set, the
        /// Transfer Event generated shall not interrupt when enqueued.
        pub block_event_interrupt: bool = 9;

        /// Set to [TrbType::Normal] for Normal TRBs.
        pub trb_type: TrbType = 10..16;

        reserved2: u16 = 16..32;
    }
}

bitstruct! {
    /// Setup Stage TRB control fields (xHCI 1.2 table 6-26)
    #[derive(Clone, Copy, Debug, Default, FromBytes, Immutable)]
    pub struct TrbControlFieldSetupStage(pub u32) {
        /// Used to mark the Enqueue Pointer of the Transfer Ring.
        pub cycle: bool = 0;

        reserved1: u8 = 1..5;

        /// Or "IOC". See [TrbControlFieldNormal::interrupt_on_completion]
        pub interrupt_on_completion: bool = 5;

        /// Or "IDT". See [TrbControlFieldNormal::immediate_data]
        pub immediate_data: bool = 6;

        reserved2: u8 = 7..10;

        /// Set to [TrbType::SetupStage] for Setup Stage TRBs.
        pub trb_type: TrbType = 10..16;

        /// Or "TRT". Indicates the type and direction of the control transfer.
        pub transfer_type: TrbTransferType = 16..18;

        reserved3: u16 = 18..32;
    }
}

bitstruct! {
    /// Data Stage TRB control fields (xHCI 1.2 table 6-29)
    #[derive(Clone, Copy, Debug, Default, FromBytes, Immutable)]
    pub struct TrbControlFieldDataStage(pub u32) {
        /// Used to mark the Enqueue Pointer of the Transfer Ring.
        pub cycle: bool = 0;

        /// Or "ENT". See [TrbControlFieldNormal::evaluate_next_trb]
        pub evaluate_next_trb: bool = 1;

        /// Or "ISP". See [TrbControlFieldNormal::interrupt_on_short_packet]
        pub interrupt_on_short_packet: bool = 2;

        /// Or "NS". See [TrbControlFieldNormal::no_snoop]
        pub no_snoop: bool = 3;

        /// Or "CH". See [TrbControlFieldNormal::chain_bit]
        pub chain_bit: bool = 4;

        /// Or "IOC". See [TrbControlFieldNormal::interrupt_on_completion]
        pub interrupt_on_completion: bool = 5;

        /// Or "IDT". See [TrbControlFieldNormal::immediate_data]
        pub immediate_data: bool = 6;

        reserved1: u8 = 7..10;

        /// Set to [TrbType::DataStage] for Data Stage TRBs.
        pub trb_type: TrbType = 10..16;

        /// Or "DIR". Indicates the direction of data transfer, where
        /// OUT (0) is toward the device and IN (1) is toward the host.
        /// (see xHCI 1.2 sect 4.11.2.2)
        pub direction: TrbDirection = 16;

        reserved2: u16 = 17..32;
    }
}

bitstruct! {
    /// Status Stage TRB control fields (xHCI 1.2 table 6-31)
    #[derive(Clone, Copy, Debug, Default, FromBytes, Immutable)]
    pub struct TrbControlFieldStatusStage(pub u32) {
        /// Used to mark the Enqueue Pointer of the Transfer Ring.
        pub cycle: bool = 0;

        /// Or "ENT". If set, the xHC shall fetch and evaluate the next TRB
        /// before saving the endpoint state (see xHCI 1.2 Section 4.12.3)
        pub evaluate_next_trb: bool = 1;

        reserved1: u8 = 2..4;

        /// Or "CH". See [TrbControlFieldNormal::chain_bit]
        pub chain_bit: bool = 4;

        /// Or "IOC". See [TrbControlFieldNormal::interrupt_on_completion]
        pub interrupt_on_completion: bool = 5;

        reserved2: u8 = 6..10;

        /// Set to [TrbType::StatusStage] for Status Stage TRBs.
        pub trb_type: TrbType = 10..16;

        /// Or "DIR". See [TrbControlFieldDataStage::direction]
        pub direction: TrbDirection = 16;

        reserved3: u16 = 17..32;
    }
}

bitstruct! {
    /// Status Stage TRB control fields (xHCI 1.2 table 6-31)
    #[derive(Clone, Copy, Debug, Default, FromBytes, Immutable)]
    pub struct TrbControlFieldLink(pub u32) {
        /// Used to mark the Enqueue Pointer of the Transfer or Command Ring.
        pub cycle: bool = 0;

        /// Or "TC". If set, the xHC shall toggle its interpretation of the
        /// cycle bit. If claered, the xHC shall continue to the next segment
        /// using its current cycle bit interpretation.
        pub toggle_cycle: bool = 1;

        reserved1: u8 = 2..4;

        /// Or "CH". See [TrbControlFieldNormal::chain_bit]
        pub chain_bit: bool = 4;

        /// Or "IOC". See [TrbControlFieldNormal::interrupt_on_completion]
        pub interrupt_on_completion: bool = 5;

        reserved2: u8 = 6..10;

        /// Set to [TrbType::Link] for Link TRBs.
        pub trb_type: TrbType = 10..16;

        reserved3: u16 = 16..32;
    }
}

bitstruct! {
    /// Common control fields in Event TRBs
    #[derive(Clone, Copy, Debug, Default, FromBytes, Immutable)]
    pub struct TrbControlFieldEvent(pub u32) {
        /// Used to mark the Enqueue Pointer of the Transfer or Command Ring.
        pub cycle: bool = 0;

        reserved1: u16 = 1..10;

        // Set to the [TrbType] corresponding to the Event.
        pub trb_type: TrbType = 10..16;

        pub virtual_function_id: u8 = 16..24;

        /// ID of the Device Slot corresponding to this event.
        pub slot_id: SlotId = 24..32;
    }
}

bitstruct! {
    /// Common control fields in Transfer Event TRBs
    #[derive(Clone, Copy, Debug, Default, FromBytes, Immutable)]
    pub struct TrbControlFieldTransferEvent(pub u32) {
        /// Used to mark the Enqueue Pointer of the Transfer or Command Ring.
        pub cycle: bool = 0;

        reserved0: bool = 1;

        /// Or "ED". If set, event was generated by an Event Data TRB and the
        /// parameter is a 64-bit value provided by such. If cleared (0), the
        /// parameter is a pointer to the TRB that generated this event.
        /// (See xHCI 1.2 sect 4.11.5.2)
        pub event_data: bool = 2;

        reserved1: u16 = 3..10;

        /// Set to [TrbType::TransferEvent] for Transfer Event TRBs.
        pub trb_type: TrbType = 10..16;

        /// ID of the Endpoint that generated the event. Used as an index in
        /// the Device Context to select the Endpoint Context associated with
        /// this Event.
        pub endpoint_id: u8 = 16..21;

        reserved2: u16 = 21..24;

        /// ID of the Device Slot corresponding to this event.
        pub slot_id: SlotId = 24..32;
    }
}

bitstruct! {
    /// Common control fields in Command TRBs to do with slot enablement
    #[derive(Clone, Copy, Debug, Default, FromBytes, Immutable)]
    pub struct TrbControlFieldSlotCmd(pub u32) {
        /// Used to mark the Enqueue Pointer of the Transfer or Command Ring.
        pub cycle: bool = 0;

        reserved1: u16 = 1..9;

        /// In an Address Device Command TRB, this is BSR (Block SetAddress Request).
        /// When true, the Address Device Command shall not generate a USB
        /// SET_ADDRESS request. (xHCI 1.2 section 4.6.5, table 6-62)
        ///
        /// In a Configure Endpoint Command TRB, this is DC (Deconfigure).
        pub bit9: bool = 9;

        /// Set to either [TrbType::EnableSlotCmd] or [TrbType::DisableSlotCmd]
        pub trb_type: TrbType = 10..16;

        /// Type of Slot to be enabled by this command. (See xHCI 1.2 table 7-9)
        pub slot_type: u8 = 16..21;

        reserved2: u8 = 21..24;

        /// ID of the Device Slot corresponding to this event.
        pub slot_id: SlotId = 24..32;
    }
}

bitstruct! {
    /// Common control fields in Command TRBs to do with endpoint start/stop/reset
    #[derive(Clone, Copy, Debug, Default, FromBytes, Immutable)]
    pub struct TrbControlFieldEndpointCmd(pub u32) {
        /// Used to mark the Enqueue Pointer of the Transfer or Command Ring.
        pub cycle: bool = 0;

        reserved1: u16 = 1..9;

        /// Only in Reset Endpoint Command TRB.
        /// If true, the Reset operation doesn't affect the current transfer
        /// state of the endpoint. (See also xHCI 1.2 sect 4.6.8.1)
        pub transfer_state_preserve: bool = 9;

        /// [TrbType::ConfigureEndpointCmd], [TrbType::ResetEndpointCmd],
        /// or [TrbType::StopEndpointCmd].
        pub trb_type: TrbType = 10..16;

        /// The Device Context Index (xHCI 1.2 section 4.8.1) of the EP Context.
        /// Valid values are 1..=31.
        pub endpoint_id: u8 = 16..21;

        reserved2: u8 = 21..23;

        /// Only in Stop Endpoint Command TRB.
        /// If true, we're stopping activity on an endpoint that's about to be
        /// suspended, and the endpoint shall be stopped for at least 10ms.
        pub suspend: bool = 23;

        /// ID of the Device Slot corresponding to this event.
        pub slot_id: SlotId = 24..32;
    }
}

bitstruct! {
    /// Control fields of Get Port Bandwidth Command TRB
    #[derive(Clone, Copy, Debug, Default, FromBytes, Immutable)]
    pub struct TrbControlFieldGetPortBandwidthCmd(pub u32) {
        /// Used to mark the Enqueue Pointer of the Transfer or Command Ring.
        pub cycle: bool = 0;

        reserved1: u16 = 1..9;

        /// Only in Reset Endpoint Command TRB.
        /// If true, the Reset operation doesn't affect the current transfer
        /// state of the endpoint. (See also xHCI 1.2 sect 4.6.8.1)
        pub transfer_state_preserve: bool = 9;

        /// Set to [TrbType::GetPortBandwidthCmd]
        pub trb_type: TrbType = 10..16;

        /// The bus speed of interest (See 'Port Speed' in xHCI 1.2 table 5-27,
        /// but no Undefined or Reserved speeds allowed here)
        pub dev_speed: u8 = 16..20;

        reserved2: u8 = 20..24;

        /// ID of the Hub Slot of which the bandwidth shall be returned.
        pub hub_slot_id: SlotId = 24..32;
    }
}

bitstruct! {
    /// Common control fields in Command TRBs to do with extended properties
    #[derive(Clone, Copy, Debug, Default, FromBytes, Immutable)]
    pub struct TrbControlFieldExtendedPropsCmd(pub u32) {
        /// Used to mark the Enqueue Pointer of the Transfer or Command Ring.
        pub cycle: bool = 0;

        reserved1: u16 = 1..10;

        /// Set to [TrbType::GetExtendedPropertyCmd] or
        /// [TrbType::SetExtendedPropertyCmd]
        pub trb_type: TrbType = 10..16;

        /// Indicates the specific extended capability specific action the xHC
        /// is required to perform. Software sets to 0 when the ECI is 0
        pub subtype: u8 = 16..19;

        /// ID of the Endpoint whose extended properties we're interested in.
        /// If nonzero, `slot_id` shall be valid.
        pub endpoint_id: u8 = 19..24;

        /// ID of the Device Slot whose extended properties we're interested in.
        pub slot_id: SlotId = 24..32;
    }
}

#[derive(Copy, Clone, FromBytes, Immutable)]
pub union TrbStatusField {
    pub transfer: TrbStatusFieldTransfer,
    pub event: TrbStatusFieldEvent,
    pub command_ext: TrbStatusFieldCommandExtProp,
}
impl Default for TrbStatusField {
    fn default() -> Self {
        Self { transfer: TrbStatusFieldTransfer(0) }
    }
}

bitstruct! {
    /// Representation of the 'status' field of Transfer Request Block (TRB).
    ///
    /// See xHCI 1.2 Section 6.4.1 (Comments are paraphrases thereof)
    #[derive(Clone, Copy, Debug, Default, FromBytes, Immutable)]
    pub struct TrbStatusFieldTransfer(pub u32) {
        /// For OUT, this field defines the number of data bytes the xHC shall
        /// send during the execution of this TRB. If this field is 0 when the
        /// xHC fetches this TRB, xHC shall execute a zero-length transaction.
        /// (See xHCI 1.2 section 4.9.1 for zero-length TRB handling)
        ///
        /// For IN, this field indicates the size of the data buffer referenced
        /// by the Data Buffer Pointer, i.e. the number of bytes the host
        /// expects the endpoint to deliver.
        ///
        /// "Valid values are 0 to 64K."
        pub trb_transfer_length: u32 = 0..17;

        /// Indicates number of packets remaining in the Transfer Descriptor.
        /// (See xHCI 1.2 section 4.10.2.4)
        pub td_size: u8 = 17..22;

        /// The index of the Interrupter that will receive events generated
        /// by this TRB. "Valid values are between 0 and MaxIntrs-1."
        pub interrupter_target: u16 = 22..32;
    }
}

bitstruct! {
    #[derive(Clone, Copy, Debug, Default, FromBytes, Immutable)]
    pub struct TrbStatusFieldEvent(pub u32) {
        /// Optionally set by a command, see xHCI 1.2 sect 4.6.6.1.
        pub completion_parameter: u32 = 0..24;

        /// The completion status of the command that generated the event.
        /// See xHCI 1.2 section 6.4.5, as well as the specifications for each
        /// individual command's behavior in section 4.6.
        pub completion_code: TrbCompletionCode = 24..32;
    }
}

bitstruct! {
    #[derive(Clone, Copy, Debug, Default, FromBytes, Immutable)]
    pub struct TrbStatusFieldCommandExtProp(pub u32) {
        /// ECI. Specifies the Extended Capability Identifier associated
        /// with the Get/Set Extended Property Command (See xHCI 1.2 table 4-3)
        pub extended_capability_id: u16 = 0..16;

        /// In *Set* Extended Property Command TRB, specifies a parameter to be
        /// interpreted by the xHC based on the given ECI.
        pub capability_parameter: u8 = 16..24;

        reserved: u8 = 24..32;
    }
}

/// xHCI 1.2 Section 6.4.6
#[derive(FromRepr, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
#[repr(u8)]
pub enum TrbType {
    Reserved0 = 0,
    Normal = 1,
    SetupStage = 2,
    DataStage = 3,
    StatusStage = 4,
    Isoch = 5,
    Link = 6,
    EventData = 7,
    NoOp = 8,
    EnableSlotCmd = 9,
    DisableSlotCmd = 10,
    AddressDeviceCmd = 11,
    ConfigureEndpointCmd = 12,
    EvaluateContextCmd = 13,
    ResetEndpointCmd = 14,
    StopEndpointCmd = 15,
    SetTRDequeuePointerCmd = 16,
    ResetDeviceCmd = 17,
    ForceEventCmd = 18,
    NegotiateBandwidthCmd = 19,
    SetLatencyToleranceValueCmd = 20,
    GetPortBandwidthCmd = 21,
    ForceHeaderCmd = 22,
    NoOpCmd = 23,
    GetExtendedPropertyCmd = 24,
    SetExtendedPropertyCmd = 25,
    Reserved26 = 26,
    Reserved27 = 27,
    Reserved28 = 28,
    Reserved29 = 29,
    Reserved30 = 30,
    Reserved31 = 31,
    TransferEvent = 32,
    CommandCompletionEvent = 33,
    PortStatusChangeEvent = 34,
    BandwidthRequestEvent = 35,
    DoorbellEvent = 36,
    HostControllerEvent = 37,
    DeviceNotificationEvent = 38,
    MfIndexWrapEvent = 39,
    Reserved40 = 40,
    Reserved41 = 41,
    Reserved42 = 42,
    Reserved43 = 43,
    Reserved44 = 44,
    Reserved45 = 45,
    Reserved46 = 46,
    Reserved47 = 47,
    Vendor48 = 48,
    Vendor49 = 49,
    Vendor50 = 50,
    Vendor51 = 51,
    Vendor52 = 52,
    Vendor53 = 53,
    Vendor54 = 54,
    Vendor55 = 55,
    Vendor56 = 56,
    Vendor57 = 57,
    Vendor58 = 58,
    Vendor59 = 59,
    Vendor60 = 60,
    Vendor61 = 61,
    Vendor62 = 62,
    Vendor63 = 63,
}

impl From<u8> for TrbType {
    fn from(value: u8) -> Self {
        Self::from_repr(value).expect("TrbType should only be converted from a 6-bit field in TrbControlField")
    }
}
impl Into<u8> for TrbType {
    fn into(self) -> u8 {
        self as u8
    }
}

/// Or "TRT". See xHCI 1.2 Table 6-26 and Section 4.11.2.2
#[derive(FromRepr, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
#[repr(u8)]
pub enum TrbTransferType {
    NoDataStage = 0,
    Reserved = 1,
    OutDataStage = 2,
    InDataStage = 3,
}
impl From<u8> for TrbTransferType {
    fn from(value: u8) -> Self {
        Self::from_repr(value).expect("TrbTransferType should only be converted from a 2-bit field in TrbControlField")
    }
}
impl Into<u8> for TrbTransferType {
    fn into(self) -> u8 {
        self as u8
    }
}

#[derive(FromRepr, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
#[repr(u8)]
pub enum TrbDirection {
    Out = 0,
    In = 1,
}
impl From<bool> for TrbDirection {
    fn from(value: bool) -> Self {
        unsafe { core::mem::transmute(value as u8) }
    }
}
impl Into<bool> for TrbDirection {
    fn into(self) -> bool {
        self == Self::In
    }
}

#[derive(FromRepr, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
#[repr(u8)]
pub enum TrbCompletionCode {
    Invalid = 0,
    Success = 1,
    DataBufferError = 2,
    BabbleDetectedError = 3,
    UsbTransactionError = 4,
    TrbError = 5,
    StallError = 6,
    ResourceError = 7,
    BandwidthError = 8,
    NoSlotsAvailableError = 9,
    InvalidStreamTypeError = 10,
    SlotNotEnabledError = 11,
    EndpointNotEnabledError = 12,
    ShortPacket = 13,
    RingUnderrun = 14,
    RingOverrun = 15,
    VfEventRingFullError = 16,
    ParameterError = 17,
    BandwidthOverrunError = 18,
    ContextStateError = 19,
    NoPingResponseError = 20,
    EventRingFullError = 21,
    IncompatibleDeviceError = 22,
    MissedServiceError = 23,
    CommandRingStopped = 24,
    CommandAborted = 25,
    Stopped = 26,
    StoppedLengthInvalid = 27,
    StoppedShortPacket = 28,
    MaxExitLatencyTooLarge = 29,
    Reserved30 = 30,
    IsochBufferOverrun = 31,
    EventLostError = 32,
    UndefinedError = 33,
    InvalidStreamIdError = 34,
    SecondaryBandwidthError = 35,
    SplitTransactionError = 36,
    Reserved37 = 37,
    Reserved38 = 38,
    Reserved39 = 39,
    Reserved40 = 40,
    Reserved41 = 41,
    Reserved42 = 42,
    Reserved43 = 43,
    Reserved44 = 44,
    Reserved45 = 45,
    Reserved46 = 46,
    Reserved47 = 47,
    Reserved48 = 48,
    Reserved49 = 49,
    Reserved50 = 50,
    Reserved51 = 51,
    Reserved52 = 52,
    Reserved53 = 53,
    Reserved54 = 54,
    Reserved55 = 55,
    Reserved56 = 56,
    Reserved57 = 57,
    Reserved58 = 58,
    Reserved59 = 59,
    Reserved60 = 60,
    Reserved61 = 61,
    Reserved62 = 62,
    Reserved63 = 63,
    Reserved64 = 64,
    Reserved65 = 65,
    Reserved66 = 66,
    Reserved67 = 67,
    Reserved68 = 68,
    Reserved69 = 69,
    Reserved70 = 70,
    Reserved71 = 71,
    Reserved72 = 72,
    Reserved73 = 73,
    Reserved74 = 74,
    Reserved75 = 75,
    Reserved76 = 76,
    Reserved77 = 77,
    Reserved78 = 78,
    Reserved79 = 79,
    Reserved80 = 80,
    Reserved81 = 81,
    Reserved82 = 82,
    Reserved83 = 83,
    Reserved84 = 84,
    Reserved85 = 85,
    Reserved86 = 86,
    Reserved87 = 87,
    Reserved88 = 88,
    Reserved89 = 89,
    Reserved90 = 90,
    Reserved91 = 91,
    Reserved92 = 92,
    Reserved93 = 93,
    Reserved94 = 94,
    Reserved95 = 95,
    Reserved96 = 96,
    Reserved97 = 97,
    Reserved98 = 98,
    Reserved99 = 99,
    Reserved100 = 100,
    Reserved101 = 101,
    Reserved102 = 102,
    Reserved103 = 103,
    Reserved104 = 104,
    Reserved105 = 105,
    Reserved106 = 106,
    Reserved107 = 107,
    Reserved108 = 108,
    Reserved109 = 109,
    Reserved110 = 110,
    Reserved111 = 111,
    Reserved112 = 112,
    Reserved113 = 113,
    Reserved114 = 114,
    Reserved115 = 115,
    Reserved116 = 116,
    Reserved117 = 117,
    Reserved118 = 118,
    Reserved119 = 119,
    Reserved120 = 120,
    Reserved121 = 121,
    Reserved122 = 122,
    Reserved123 = 123,
    Reserved124 = 124,
    Reserved125 = 125,
    Reserved126 = 126,
    Reserved127 = 127,
    Reserved128 = 128,
    Reserved129 = 129,
    Reserved130 = 130,
    Reserved131 = 131,
    Reserved132 = 132,
    Reserved133 = 133,
    Reserved134 = 134,
    Reserved135 = 135,
    Reserved136 = 136,
    Reserved137 = 137,
    Reserved138 = 138,
    Reserved139 = 139,
    Reserved140 = 140,
    Reserved141 = 141,
    Reserved142 = 142,
    Reserved143 = 143,
    Reserved144 = 144,
    Reserved145 = 145,
    Reserved146 = 146,
    Reserved147 = 147,
    Reserved148 = 148,
    Reserved149 = 149,
    Reserved150 = 150,
    Reserved151 = 151,
    Reserved152 = 152,
    Reserved153 = 153,
    Reserved154 = 154,
    Reserved155 = 155,
    Reserved156 = 156,
    Reserved157 = 157,
    Reserved158 = 158,
    Reserved159 = 159,
    Reserved160 = 160,
    Reserved161 = 161,
    Reserved162 = 162,
    Reserved163 = 163,
    Reserved164 = 164,
    Reserved165 = 165,
    Reserved166 = 166,
    Reserved167 = 167,
    Reserved168 = 168,
    Reserved169 = 169,
    Reserved170 = 170,
    Reserved171 = 171,
    Reserved172 = 172,
    Reserved173 = 173,
    Reserved174 = 174,
    Reserved175 = 175,
    Reserved176 = 176,
    Reserved177 = 177,
    Reserved178 = 178,
    Reserved179 = 179,
    Reserved180 = 180,
    Reserved181 = 181,
    Reserved182 = 182,
    Reserved183 = 183,
    Reserved184 = 184,
    Reserved185 = 185,
    Reserved186 = 186,
    Reserved187 = 187,
    Reserved188 = 188,
    Reserved189 = 189,
    Reserved190 = 190,
    Reserved191 = 191,
    VendorDefinedError192 = 192,
    VendorDefinedError193 = 193,
    VendorDefinedError194 = 194,
    VendorDefinedError195 = 195,
    VendorDefinedError196 = 196,
    VendorDefinedError197 = 197,
    VendorDefinedError198 = 198,
    VendorDefinedError199 = 199,
    VendorDefinedError200 = 200,
    VendorDefinedError201 = 201,
    VendorDefinedError202 = 202,
    VendorDefinedError203 = 203,
    VendorDefinedError204 = 204,
    VendorDefinedError205 = 205,
    VendorDefinedError206 = 206,
    VendorDefinedError207 = 207,
    VendorDefinedError208 = 208,
    VendorDefinedError209 = 209,
    VendorDefinedError210 = 210,
    VendorDefinedError211 = 211,
    VendorDefinedError212 = 212,
    VendorDefinedError213 = 213,
    VendorDefinedError214 = 214,
    VendorDefinedError215 = 215,
    VendorDefinedError216 = 216,
    VendorDefinedError217 = 217,
    VendorDefinedError218 = 218,
    VendorDefinedError219 = 219,
    VendorDefinedError220 = 220,
    VendorDefinedError221 = 221,
    VendorDefinedError222 = 222,
    VendorDefinedError223 = 223,
    VendorDefinedInfo224 = 224,
    VendorDefinedInfo225 = 225,
    VendorDefinedInfo226 = 226,
    VendorDefinedInfo227 = 227,
    VendorDefinedInfo228 = 228,
    VendorDefinedInfo229 = 229,
    VendorDefinedInfo230 = 230,
    VendorDefinedInfo231 = 231,
    VendorDefinedInfo232 = 232,
    VendorDefinedInfo233 = 233,
    VendorDefinedInfo234 = 234,
    VendorDefinedInfo235 = 235,
    VendorDefinedInfo236 = 236,
    VendorDefinedInfo237 = 237,
    VendorDefinedInfo238 = 238,
    VendorDefinedInfo239 = 239,
    VendorDefinedInfo240 = 240,
    VendorDefinedInfo241 = 241,
    VendorDefinedInfo242 = 242,
    VendorDefinedInfo243 = 243,
    VendorDefinedInfo244 = 244,
    VendorDefinedInfo245 = 245,
    VendorDefinedInfo246 = 246,
    VendorDefinedInfo247 = 247,
    VendorDefinedInfo248 = 248,
    VendorDefinedInfo249 = 249,
    VendorDefinedInfo250 = 250,
    VendorDefinedInfo251 = 251,
    VendorDefinedInfo252 = 252,
    VendorDefinedInfo253 = 253,
    VendorDefinedInfo254 = 254,
    VendorDefinedInfo255 = 255,
}

impl From<u8> for TrbCompletionCode {
    fn from(value: u8) -> Self {
        // the field is 8-bits and the entire range is defined in the enum
        unsafe { core::mem::transmute(value) }
    }
}
impl Into<u8> for TrbCompletionCode {
    fn into(self) -> u8 {
        self as u8
    }
}
