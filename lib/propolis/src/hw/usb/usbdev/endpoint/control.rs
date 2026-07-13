// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use core::fmt::Debug;
use std::{marker::PhantomData, sync::Arc};

use crate::common::{GuestData, GuestRegion};
use crate::hw::usb::usbdev::descriptor::{ConfigurationValue, DescriptorType};
use crate::hw::usb::usbdev::requests::{
    RequestDirection, RequestType, SetupData, StandardRequest,
};
use crate::hw::usb::usbdev::{Error, Result};
use crate::hw::usb::xhci::bits::ring_data::TrbCompletionCode;
use crate::hw::usb::xhci::controller::XhciPortHandle;
use crate::hw::usb::xhci::device_slots::{EndpointId, SlotId};
use crate::hw::usb::xhci::rings::consumer::transfer::{
    PointerOrImmediate, TransferTrb,
};
use crate::vmm::MemCtx;

#[usdt::provider(provider = "propolis")]
mod probes {
    fn usb_control_xfer_setup(
        slot_id: u8,
        endpoint_id: u8,
        request_type: u8,
        request: u8,
        device_to_host: bool,
    ) {
    }
    fn usb_control_xfer_data(slot_id: u8, endpoint_id: u8, trb_pointer: u64) {}
    fn usb_control_xfer_status(slot_id: u8, endpoint_id: u8, ok: bool) {}
}

/// Abstraction for handling USB Control Transfers, most prominently used for
/// requests sent by the guest to the Default Control Endpoint.
/// When a Control Endpoint's doorbell register is rung and the associated
/// [TransferRing] contains Setup Stage, Data Stage, or Status Stage TRB(s),
/// they are sent to their respective methods on the [UsbDevice], which in turn
/// may implement their handling by passing them to the corresponding methods
/// of this struct.
///
/// [UsbDevice]: crate::hw::usb::usbdev::UsbDevice
/// [TransferRing]: crate::hw::usb::xhci::rings::consumer::transfer::TransferRing
pub struct ControlEndpoint<C>
where
    ControlRequestInfo<C>: TryFrom<SetupData, Error = Error>,
    C: TryFrom<SetupData, Error = Error> + Debug,
{
    current_setup: Option<SetupData>,
    payload: Option<Vec<u8>>,
    bytes_transferred: usize,
    slot_id: SlotId,
    endpoint_id: EndpointId,
    port_hdl: Arc<XhciPortHandle>,
    log: slog::Logger,
    _spooky: PhantomData<C>,
}

pub struct ControlEPStatusStageResult<
    'a,
    C: TryFrom<SetupData, Error = Error> + Debug,
> {
    pub request: ControlRequestInfo<C>,
    pub payload: Option<&'a [u8]>,
}

impl<C> ControlEndpoint<C>
where
    ControlRequestInfo<C>: TryFrom<SetupData, Error = Error>,
    C: TryFrom<SetupData, Error = Error> + Debug,
{
    pub fn new(
        slot_id: SlotId,
        endpoint_id: EndpointId,
        port_hdl: Arc<XhciPortHandle>,
        log: &slog::Logger,
    ) -> Self {
        Self {
            current_setup: None,
            payload: None,
            bytes_transferred: 0,
            slot_id,
            endpoint_id,
            port_hdl,
            log: log.new(slog::o!(
                "endpoint_type" => "control",
                "endpoint_id" => u8::from(endpoint_id)
            )),
            _spooky: PhantomData,
        }
    }

    pub fn new_migrated(
        value: &migrate::ControlEndpointV1,
        port_hdl: Arc<XhciPortHandle>,
        log: &slog::Logger,
    ) -> Self {
        let mut new = Self::new(
            SlotId::from(value.slot_id),
            EndpointId::from(value.endpoint_id),
            port_hdl,
            log,
        );
        new.import(&value);
        new
    }

    /// Set data to send the guest in [RequestDirection::DeviceToHost] request.
    pub fn set_payload(&mut self, payload: Vec<u8>) -> Result<()> {
        let Some(setup) = &self.current_setup else {
            return Err(Error::GavePayloadBeforeRequest(payload));
        };
        if setup.direction() == RequestDirection::HostToDevice {
            Err(Error::GavePayloadForOutRequest(setup.request(), payload))
        } else if let Some(existing) = &self.payload {
            Err(Error::GavePayloadTwice(existing.to_owned(), payload))
        } else {
            self.payload = Some(payload);
            Ok(())
        }
    }

    /// Called when we receive a Setup Stage TRB from the guest, this prepares
    /// our USB device for the Data Stage(s) that follow. The Setup Stage gives
    /// the [RequestDirection] of data transfer and information about what is
    /// being request (e.g. a GET_DESCRIPTOR request with the ID of a type of
    /// [Descriptor] as its value, or a class-specific request type).
    ///
    /// Replaces any previous request-related state on this endpoint.
    ///
    /// [Descriptor]: crate::hw::usb::usbdev::descriptor::Descriptor
    pub fn setup_stage(
        &mut self,
        setup: SetupData,
    ) -> Result<Option<ControlRequestInfo<C>>> {
        self.bytes_transferred = 0;
        self.current_setup = Some(setup);
        self.payload = None;
        let control_request_info = match setup.direction() {
            RequestDirection::DeviceToHost => {
                Some(ControlRequestInfo::try_from(setup)?)
            }
            RequestDirection::HostToDevice => None,
        };
        probes::usb_control_xfer_setup!(|| (
            u8::from(self.slot_id),
            u8::from(self.endpoint_id),
            setup.request_type() as u8,
            setup.request(),
            control_request_info.is_some(),
        ));
        Ok(control_request_info)
    }

    /// Transfers the requested data to or from guest memory at the address(es)
    /// given in [TransferTrb]s, according to the request information previously
    /// specified in the Setup Stage.
    ///
    /// For [RequestDirection::DeviceToHost] transfers, the payload is written
    /// progressively into each TRB if it does not fit within the first, if
    /// multiple [TransferTrb]s are provided.
    ///
    /// For [RequestDirection::HostToDevice] transfers, the payload accumulates
    /// data from each [TransferTrb] provided.
    ///
    /// Unlike [ControlEndpoint::setup_stage] and [ControlEndpoint::status_stage],
    /// this method puts its own completion events into the Event Ring for each
    /// successful Transfer TRB.
    pub fn data_stage(
        &mut self,
        xfer_trbs: &[TransferTrb],
        data_direction: RequestDirection,
        memctx: &MemCtx,
    ) -> Result<()> {
        let Some(setup_data) = self.current_setup.as_ref() else {
            return Err(Error::NoSetupStageBefore("Data Stage"));
        };
        if data_direction != setup_data.direction() {
            return Err(Error::SetupVsDataDirectionMismatch(
                setup_data.direction(),
                data_direction,
            ));
        }
        for trb in xfer_trbs {
            let count = match setup_data.direction() {
                RequestDirection::DeviceToHost => {
                    let Some(payload) = &self.payload else {
                        return Err(Error::MissingPayloadForInRequest(
                            setup_data.request(),
                        ));
                    };
                    let PointerOrImmediate::Pointer(region) = trb.data_buffer()
                    else {
                        return Err(Error::ImmediateParameterForInTransfer);
                    };
                    memctx
                        .write_from(
                            region.0,
                            &payload[self.bytes_transferred..],
                            region.1,
                        )
                        .ok_or(Error::DataStageWriteFailed)?
                }
                RequestDirection::HostToDevice => {
                    let payload = self.payload.get_or_insert_default();
                    match trb.data_buffer() {
                        PointerOrImmediate::Pointer(GuestRegion(ptr, len)) => {
                            let bt = self.bytes_transferred;
                            payload.resize(bt + len, 0u8);
                            memctx
                                .read_into(
                                    ptr,
                                    &mut GuestData::from(&mut payload[bt..]),
                                    len,
                                )
                                .ok_or(Error::DataStageReadFailed)?
                        }
                        PointerOrImmediate::Immediate(arr, len) => {
                            payload.extend_from_slice(&arr[..len]);
                            len
                        }
                    }
                }
            };
            probes::usb_control_xfer_data!(|| (
                u8::from(self.slot_id),
                u8::from(self.endpoint_id),
                trb.trb_pointer().0,
            ));
            if let Err(e) = self.port_hdl.send_completion_events_for_trb(
                trb,
                TrbCompletionCode::Success,
                count,
                self.slot_id,
                self.endpoint_id,
            ) {
                slog::error!(
                    self.log,
                    "Failed to send completion events for USB Control transfer: {e}"
                );
            }
            self.bytes_transferred += count;
        }
        Ok(())
    }

    /// Called when the Status Stage TRB is received from the guest.
    ///
    /// For [RequestDirection::HostToDevice] transfers, we return the payload
    /// to the emulated USB device for it to handle according to the type of
    /// request we were given in the Status Stage.
    pub fn status_stage(
        &mut self,
        status_direction: RequestDirection,
    ) -> Result<Option<ControlEPStatusStageResult<'_, C>>> {
        let Some(setup) = self.current_setup.take() else {
            return Err(Error::NoSetupStageBefore("Status Stage"));
        };
        if status_direction == setup.direction() {
            return Err(Error::SetupVsStatusDirectionMatch(status_direction));
        }

        let result = match setup.direction() {
            RequestDirection::HostToDevice => {
                Some(ControlRequestInfo::try_from(setup).map(|request| {
                    ControlEPStatusStageResult {
                        request,
                        payload: self.payload.as_deref(),
                    }
                }))
                .transpose()
            }
            RequestDirection::DeviceToHost => Ok(None),
        };

        probes::usb_control_xfer_status!(|| (
            u8::from(self.slot_id),
            u8::from(self.endpoint_id),
            result.is_ok(),
        ));
        self.bytes_transferred = 0;
        result
    }

    pub fn import(&mut self, value: &migrate::ControlEndpointV1) {
        let migrate::ControlEndpointV1 {
            current_setup,
            payload,
            bytes_transferred,
            slot_id,
            endpoint_id,
        } = value;
        self.current_setup = current_setup.map(|x| SetupData(x));
        self.payload = payload.to_owned();
        self.bytes_transferred = *bytes_transferred;
        self.slot_id = SlotId::from(*slot_id);
        self.endpoint_id = EndpointId::from(*endpoint_id);
    }

    pub fn export(&self) -> super::migrate::EndpointV1 {
        let Self {
            current_setup,
            payload,
            bytes_transferred,
            slot_id,
            endpoint_id,
            _spooky,
            port_hdl: _,
            log: _,
        } = self;
        super::migrate::EndpointV1::Control(migrate::ControlEndpointV1 {
            current_setup: current_setup.as_ref().map(|x| x.0),
            payload: payload.to_owned(),
            bytes_transferred: *bytes_transferred,
            slot_id: u8::from(*slot_id),
            endpoint_id: u8::from(*endpoint_id),
        })
    }
}

#[derive(Debug)]
pub enum ControlRequestInfo<C>
where
    C: TryFrom<SetupData, Error = Error> + Debug,
{
    /// Returns the specified [DescriptorType] if one exists in the device.
    /// USB 2.0 sect 9.4.3.
    GetDescriptor { descriptor_type: DescriptorType, index: u8 },
    /// Returns the device's status.
    /// USB 2.0 sect 9.4.5.
    GetStatus,
    /// Sets the device configuration.
    /// USB 2.0 sect 9.4.7
    SetConfiguration {
        /// The lower byte of the wValue field specifies the desired
        /// configuration. This configuration value must be zero or match a
        /// configuration value from a configuration descriptor. If the
        /// configuration value is zero, the device is placed in its Address
        /// state. The upper byte of the wValue field is reserved.
        configuration: ConfigurationValue,
    },

    /// Device-class-specific request type, modeled in generic type C
    Class(C),
}

impl<C> TryFrom<SetupData> for ControlRequestInfo<C>
where
    C: TryFrom<SetupData, Error = Error> + Debug,
{
    type Error = Error;

    fn try_from(setup: SetupData) -> std::result::Result<Self, Self::Error> {
        match setup.request_type() {
            RequestType::Standard => {
                match StandardRequest::from_repr(setup.request()) {
                    Some(StandardRequest::GetDescriptor) => {
                        let [desc, index] = setup.value().to_be_bytes();
                        Ok(Self::GetDescriptor {
                            descriptor_type: DescriptorType::from_repr(desc)
                                .ok_or_else(|| {
                                    Error::UnknownDescriptorType(desc)
                                })?,
                            index,
                        })
                    }
                    Some(StandardRequest::GetStatus) => Ok(Self::GetStatus),
                    Some(StandardRequest::SetConfiguration) => {
                        Ok(Self::SetConfiguration {
                            configuration: ConfigurationValue(
                                setup.value() as u8
                            ),
                        })
                    }
                    _ => Err(Error::UnimplementedRequestType(
                        setup.request(),
                        setup.request_type(),
                    )),
                }
            }
            RequestType::Class => {
                Ok(ControlRequestInfo::Class(C::try_from(setup)?))
            }
            _ => Err(Error::UnimplementedRequestType(
                setup.request(),
                setup.request_type(),
            )),
        }
    }
}

/// Generic parameter for [ControlRequestInfo] for use in devices with no
/// device-class-specific requests to handle.
#[derive(Debug)]
pub struct NoClassRequestInfo;
impl TryFrom<SetupData> for NoClassRequestInfo {
    type Error = Error;

    fn try_from(setup: SetupData) -> Result<Self> {
        Err(Error::ClassRequestOnNonClassEndpoint(setup))
    }
}

pub mod migrate {
    use serde::{Deserialize, Serialize};

    #[derive(Serialize, Deserialize)]
    pub struct ControlEndpointV1 {
        pub current_setup: Option<u64>,
        pub payload: Option<Vec<u8>>,
        pub bytes_transferred: usize,
        pub slot_id: u8,
        pub endpoint_id: u8,
    }
}

#[cfg(test)]
mod test {
    use std::sync::{Arc, Condvar, Mutex};

    use crate::common::GuestAddr;
    use crate::hw::pci;
    use crate::hw::usb::test::pci_state_for_test;
    use crate::hw::usb::usbdev::descriptor::{
        ConfigurationValue, Descriptor, StringDescriptor,
    };
    use crate::hw::usb::usbdev::endpoint::control::{
        ControlEndpoint, ControlRequestInfo, NoClassRequestInfo,
    };
    use crate::hw::usb::usbdev::requests::{
        RequestDirection, RequestType, SetupData, StandardRequest,
    };
    use crate::hw::usb::xhci::bits::ring_data::{
        EventRingSegment, Trb, TrbControlField, TrbControlFieldDataStage,
        TrbStatusField, TrbStatusFieldTransfer, TrbType,
    };
    use crate::hw::usb::xhci::controller::XhciPortHandle;
    use crate::hw::usb::xhci::device_slots::{EndpointId, SlotId};
    use crate::hw::usb::xhci::interrupter::{EventSender, InterruptRegulation};
    use crate::hw::usb::xhci::rings::{
        consumer::transfer::TransferTrb, producer::event::EventRing,
    };

    const DIR_OUT: RequestDirection = RequestDirection::HostToDevice;
    const DIR_IN: RequestDirection = RequestDirection::DeviceToHost;

    // memory layout
    // 1 KiB: destination for USB data transfers
    // 6 KiB: the event ring
    // 7 KiB: the event ring segment table
    // 8 KiB: the 'transfer ring'
    struct TestScaffold {
        _log: slog::Logger,
        pci_state: Arc<pci::DeviceState>,
        _port_hdl: Arc<XhciPortHandle>,
        _interrupts: Arc<(Mutex<InterruptRegulation>, Condvar)>,
        ctrl_ep: ControlEndpoint<ControlRequestInfo<NoClassRequestInfo>>,
    }

    impl TestScaffold {
        const ERDP: GuestAddr = GuestAddr(6 * 1024);
        const ERSTBA: GuestAddr = GuestAddr(7 * 1024);
        const TRDP: GuestAddr = GuestAddr(8 * 1024);
        fn new() -> Self {
            let _log = slog::Logger::root(slog::Discard, slog::o!());
            let pci_state = pci_state_for_test();
            let memctx = pci_state.acc_mem.access().unwrap();

            memctx.write_many(
                Self::ERSTBA,
                &[EventRingSegment {
                    base_address: Self::ERDP,
                    segment_trb_count: 16,
                }],
            );
            let event_ring =
                EventRing::new(Self::ERSTBA, 1, Self::ERDP, &memctx).unwrap();

            let event_sender = Arc::new(EventSender::new(&pci_state));
            let _interrupts = Arc::new((
                Mutex::new(InterruptRegulation::new_test(
                    event_ring, &pci_state, &_log,
                )),
                Condvar::new(),
            ));
            event_sender.set_interrupts(&_interrupts);

            let _port_hdl = Arc::new(XhciPortHandle::new_test(event_sender));

            let ctrl_ep = ControlEndpoint::new(
                SlotId::from(1),
                EndpointId::from(1),
                _port_hdl.to_owned(),
                &_log,
            );

            Self { _log, _interrupts, _port_hdl, pci_state, ctrl_ep }
        }

        fn data_stage_trb(tgt_addr: GuestAddr, len: usize) -> Trb {
            Trb {
                parameter: tgt_addr.0,
                status: TrbStatusField {
                    transfer: TrbStatusFieldTransfer(0)
                        .with_trb_transfer_length(len as u32),
                },
                control: TrbControlField {
                    data_stage: TrbControlFieldDataStage(0)
                        .with_trb_type(TrbType::DataStage)
                        .with_interrupt_on_completion(true),
                },
            }
        }
    }

    /// Validate that a multi-data-stage device-to-host transfer receives the
    /// provided payload in guest memory.
    #[test]
    fn in_request() {
        let mut test = TestScaffold::new();

        let acc_mem = test.pci_state.acc_mem.child(None);
        let memctx = acc_mem.access().unwrap();
        let tgt_addr = GuestAddr(1024);

        let string = "Hey, what can you say?".to_string();

        let desc = StringDescriptor { string };

        let xfer_trbs = [
            TransferTrb::new(
                &TestScaffold::data_stage_trb(tgt_addr, 4),
                &TestScaffold::TRDP,
                None,
            )
            .unwrap(),
            TransferTrb::new(
                &TestScaffold::data_stage_trb(tgt_addr.offset::<u8>(4), 4),
                &TestScaffold::TRDP,
                None,
            )
            .unwrap(),
            TransferTrb::new(
                &TestScaffold::data_stage_trb(
                    tgt_addr.offset::<u8>(8),
                    desc.length() as usize - 8,
                ),
                &TestScaffold::TRDP,
                None,
            )
            .unwrap(),
        ];

        test.ctrl_ep
            .setup_stage(
                SetupData(0)
                    .with_request_type(RequestType::Standard)
                    .with_request(StandardRequest::GetDescriptor as u8)
                    .with_direction(DIR_IN)
                    .with_length(desc.length() as u16)
                    .with_value(u16::from_be_bytes([
                        desc.descriptor_type() as u8,
                        1,
                    ])),
            )
            .unwrap();
        test.ctrl_ep.set_payload(desc.serialize().collect()).unwrap();
        test.ctrl_ep.data_stage(&xfer_trbs, DIR_IN, &memctx).unwrap();
        assert!(test.ctrl_ep.status_stage(DIR_OUT).unwrap().is_none());

        for (i, c) in desc.string.encode_utf16().enumerate() {
            let data = memctx.read(tgt_addr.offset::<u16>(i + 1)).unwrap();
            let tada = u16::from_le(*data);
            assert_eq!(c, tada, "[{i}]: {c:#x} != {tada:#x}");
        }
    }

    /// Validate that a simple host-to-device transfer (with no data stage) is
    /// decoded correctly from the provided setup stage and returned in the
    /// subsequent status stage.
    #[test]
    fn out_request() {
        let mut test = TestScaffold::new();

        test.ctrl_ep
            .setup_stage(
                SetupData(0)
                    .with_request_type(RequestType::Standard)
                    .with_request(StandardRequest::SetConfiguration as u8)
                    .with_direction(DIR_OUT)
                    .with_value(42),
            )
            .unwrap();
        let ControlRequestInfo::SetConfiguration {
            configuration: ConfigurationValue(42),
        } = test.ctrl_ep.status_stage(DIR_IN).unwrap().unwrap().request
        else {
            panic!("mismatched wValue for SET_CONFIGURATION request")
        };
    }

    /// Verify that improper sequencing of stages, and improper use of the
    /// interface (i.e. providing device-to-host data while a host-to-device
    /// transfer is active) are treated as errors.
    #[test]
    fn wrong_request_sequence_and_direction() {
        let mut test = TestScaffold::new();
        let acc_mem = test.pci_state.acc_mem.child(None);
        let memctx = acc_mem.access().unwrap();

        // setup stage must come first
        assert!(test.ctrl_ep.set_payload(vec![]).is_err());
        assert!(test.ctrl_ep.data_stage(&[], DIR_OUT, &memctx).is_err());
        assert!(test.ctrl_ep.status_stage(DIR_IN).is_err());

        test.ctrl_ep
            .setup_stage(
                SetupData(0)
                    .with_request_type(RequestType::Standard)
                    .with_request(StandardRequest::SetConfiguration as u8)
                    .with_direction(DIR_OUT)
                    .with_value(42),
            )
            .unwrap();

        // can't set payload for OUT request
        assert!(test.ctrl_ep.set_payload(vec![]).is_err());

        // data stage direction must match
        assert!(test.ctrl_ep.data_stage(&[], DIR_IN, &memctx).is_err());

        // status stage direction must *not* match
        assert!(test.ctrl_ep.status_stage(DIR_OUT).is_err());
    }
}
