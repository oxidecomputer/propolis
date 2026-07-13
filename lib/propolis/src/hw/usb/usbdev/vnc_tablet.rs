// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

/*!

This module implements a USB HID 1.11[^hid-std] pointing device for Propolis
guests, reporting pointer state events provided by VNC clients connected to
propolis-server.

The pointer events suppiled by the RFB server are placed behind a Mutex
shared with the [InterruptInEndpoint], which has an internal state machine
for filling request buffers from the guest with payload data from VNC.

[^hid-std]: <https://www.usb.org/sites/default/files/hid1_11.pdf>

```text
      +--------------------------+
      |      HIDTabletDevice     |
      +--------------------------+
         |               |     |
 +--------------------+  |     |
 |  ControlEndpoint   |  |  +-----------------+
 |--------------------|  |  | HIDTabletReport |
 | GET_DESCRIPTOR     |  |  |-----------------|    +--------------------+
 | SET_CONFIGURATION  |  |  | Pointer state   |<---| VNC pointer events |
 | HID class-specific |  |  +-----------------+    +--------------------+
 |  (Set Idle,        |  |       |
 |   Get Report)      |  |       |
 +--------------------+  |       |
                         |       v
              +---------------------+
              | InterruptInEndpoint |
              |---------------------|
              | Receive Normal TRBs |
              | Write HID Report    |
              +---------------------+
```
 */

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, Weak};

use bitstruct::bitstruct;
use migrate::TabletDeviceV1;
use rfb::proto::{MouseButtons, PointerEvent};
use rgb_frame::Spec;

use crate::hw::ids::usb::{PROPOLIS_USB_TABLET_DEV_ID, VENDOR_OXIDE};
use crate::hw::pci;
use crate::hw::usb::usbdev::endpoint::control::ControlEPStatusStageResult;
use crate::hw::usb::xhci::bits::device_context::{
    EndpointContext, EndpointType,
};
use crate::hw::usb::xhci::bits::ring_data::TrbCompletionCode;
use crate::hw::usb::xhci::controller::XhciPortHandle;
use crate::hw::usb::xhci::device_slots::{EndpointId, SlotId};
use crate::hw::usb::xhci::rings::consumer::transfer::TransferTrb;
use crate::vmm::time::VmGuestTime;
use crate::vmm::MemCtx;

use super::descriptor::*;
use super::endpoint::control::{ControlEndpoint, ControlRequestInfo};
use super::endpoint::interrupt::{InterruptInData, InterruptInEndpoint};
use super::hid::{report::*, *};
use super::requests::{RequestDirection, SetupData};
use super::{probes, Error, Result, UsbDevice};

const REPORT_SIZE: usize = 7;

/// Struct through which data is received from VNC pointer events in order
/// to respond to USB transfers received on the Interrupt-IN endpoint (when
/// it is configured and active).
#[derive(Default)]
pub struct HIDTabletReport {
    /// Most recent HID report received is stored here in case we receive a
    /// control-endpoint Get_Report request.
    last_data: [u8; REPORT_SIZE],
    /// A reference to the Interrupt-IN endpoint's state, such that transfer
    /// requests (processed in the periodic endpoint's own thread) can be
    /// supplied data from incoming VNC pointer events
    xfer_dataref: Option<Weak<Mutex<InterruptInData>>>,
}

bitstruct! {
    struct HIDMouseButtons(pub u8) {
        pub left: bool = 0;
        pub right: bool = 1;
        pub middle: bool = 2;
    }
}

impl HIDTabletReport {
    pub fn pointer_event(&mut self, pe: PointerEvent, spec: Spec) {
        // div: spec.width and spec.height are NonZeroUsize
        let x = (pe.position.x as usize * 0x7fff / spec.width) as u16;
        let y = (pe.position.y as usize * 0x7fff / spec.height) as u16;
        // remap VNC button IDs to HID
        let mouse_left = pe.pressed.intersects(MouseButtons::LEFT);
        let mouse_middle = pe.pressed.intersects(MouseButtons::MIDDLE);
        let mouse_right = pe.pressed.intersects(MouseButtons::RIGHT);
        let scroll_up = pe.pressed.intersects(MouseButtons::SCROLL_A);
        let scroll_down = pe.pressed.intersects(MouseButtons::SCROLL_B);
        let scroll_left = pe.pressed.intersects(MouseButtons::SCROLL_C);
        let scroll_right = pe.pressed.intersects(MouseButtons::SCROLL_D);

        let button_bits = HIDMouseButtons(0)
            .with_left(mouse_left)
            .with_middle(mouse_middle)
            .with_right(mouse_right)
            .0;
        let vert_wheel = scroll_up as i8 - scroll_down as i8;
        let horiz_wheel = scroll_right as i8 - scroll_left as i8;

        let mut data = [0; REPORT_SIZE];
        for (dst, src) in data.iter_mut().zip(
            // would be nice if this filtered through the same construct that
            // generates the ReportDescriptor, should we create such a thing
            [button_bits]
                .into_iter()
                .chain(u16::to_le_bytes(x))
                .chain(u16::to_le_bytes(y))
                .chain([vert_wheel as u8, horiz_wheel as u8]),
        ) {
            *dst = src;
        }
        self.last_data = data;

        if let Some(dataref) = &self.xfer_dataref {
            if let Some(dataref) = dataref.upgrade() {
                dataref.lock().unwrap().set_payload(data.to_vec());
            }
        }
    }

    fn set_ep_data(&mut self, ep_data: Weak<Mutex<InterruptInData>>) {
        self.xfer_dataref = Some(ep_data);
    }
}

/// USB device implementing an absolute-pointing Human Interface Device.
/// See module-level documentation for overview.
pub struct HIDTabletDevice {
    control_endpoint: Option<ControlEndpoint<HIDRequestInfo>>,
    interrupt_endpoint: Option<InterruptInEndpoint>,
    slot_id: Option<SlotId>,
    ctrl_ep_id: EndpointId,
    intr_ep_id: EndpointId,
    idle_duration_4ms: u8,
    report: Arc<Mutex<HIDTabletReport>>,
    port_hdl: Arc<XhciPortHandle>,
    pci_state: Arc<pci::DeviceState>,
    guest_time: VmGuestTime,
    log: slog::Logger,
}

impl HIDTabletDevice {
    const MANUFACTURER_NAME_INDEX: StringIndex = StringIndex(1);
    const PRODUCT_NAME_INDEX: StringIndex = StringIndex(2);
    const SERIAL_INDEX: StringIndex = StringIndex(3);
    const CONFIG_NAME_INDEX: StringIndex = StringIndex(4);
    const INTERFACE_NAME_INDEX: StringIndex = StringIndex(5);

    const CONFIGURATION_VALUE: ConfigurationValue = ConfigurationValue(1);

    pub fn new(
        report: Arc<Mutex<HIDTabletReport>>,
        port_hdl: Arc<XhciPortHandle>,
        pci_state: Arc<pci::DeviceState>,
        guest_time: VmGuestTime,
        log: slog::Logger,
    ) -> Self {
        Self {
            control_endpoint: None,
            interrupt_endpoint: None,
            slot_id: None,
            ctrl_ep_id: EndpointId::from(1),
            intr_ep_id: EndpointId::from(3),
            idle_duration_4ms: 0,
            report,
            port_hdl,
            pci_state,
            guest_time,
            log,
        }
    }

    fn control_ep_mut(
        &mut self,
        endpoint_id: EndpointId,
    ) -> Result<&mut ControlEndpoint<HIDRequestInfo>> {
        if endpoint_id != self.ctrl_ep_id {
            return Err(Error::InvalidEndpoint(endpoint_id));
        }
        self.control_endpoint
            .as_mut()
            .ok_or(Error::EndpointNotConfigured(endpoint_id))
    }

    fn interrupt_ep_mut(
        &mut self,
        endpoint_id: EndpointId,
    ) -> Result<&mut InterruptInEndpoint> {
        if endpoint_id != self.intr_ep_id {
            return Err(Error::InvalidEndpoint(endpoint_id));
        }
        self.interrupt_endpoint
            .as_mut()
            .ok_or(Error::EndpointNotConfigured(endpoint_id))
    }

    fn device_descriptor() -> DeviceDescriptor {
        DeviceDescriptor {
            usb_version: USB_VER_2_0,
            // NOTE: HID class is specified in the *interface* descriptor
            device_class: ClassCode(0),
            device_subclass: SubclassCode(0),
            device_protocol: ProtocolCode(0),
            max_packet_size_0: MaxSizeZeroEP::_64,
            vendor_id: VendorId(VENDOR_OXIDE),
            product_id: ProductId(PROPOLIS_USB_TABLET_DEV_ID),
            device_version: Bcd16(0),
            manufacturer_name: Self::MANUFACTURER_NAME_INDEX,
            product_name: Self::PRODUCT_NAME_INDEX,
            serial: Self::SERIAL_INDEX,
            configurations: vec![Self::config_descriptor()],
            specific_augmentations: vec![],
        }
    }
    fn config_descriptor() -> ConfigurationDescriptor {
        ConfigurationDescriptor {
            interfaces: vec![Self::interface_descriptor()],
            config_value: Self::CONFIGURATION_VALUE,
            configuration_name: Self::CONFIG_NAME_INDEX,
            attributes: ConfigurationAttributes::default(),
            specific_augmentations: vec![],
        }
    }
    fn interface_descriptor() -> InterfaceDescriptor {
        InterfaceDescriptor {
            interface_num: 1,
            alternate_setting: 0,
            endpoints: vec![Self::interrupt_in_endpoint_descriptor()],
            class: InterfaceClass::HID,
            subclass: InterfaceSubclass(0), // no boot interface support
            protocol: InterfaceProtocol(0), // no boot interface support
            interface_name: Self::INTERFACE_NAME_INDEX,
            specific_augmentations: vec![AugmentedDescriptor::HID(
                HIDDescriptor {
                    hid_version: HID_VER_1_11,
                    country_code: CountryCode::International,
                    class_descriptor: vec![Self::report_descriptor()],
                },
            )],
        }
    }
    fn interrupt_in_endpoint_descriptor() -> EndpointDescriptor {
        EndpointDescriptor {
            endpoint_addr: 1,
            direction: Some(RequestDirection::DeviceToHost),
            attributes: EndpointAttributes::default()
                .with_transfer_type(EndpointTransferType::Interrupt),
            max_packet_size: 64,
            interval: 1,
            specific_augmentations: vec![],
        }
    }
    fn string_descriptor(idx: u8) -> StringDescriptor {
        let s: &str = match StringIndex(idx) {
            Self::MANUFACTURER_NAME_INDEX => "Oxide Computer Company",
            Self::PRODUCT_NAME_INDEX => "Propolis HID Tablet",
            Self::SERIAL_INDEX => "9002",
            Self::CONFIG_NAME_INDEX => "Absolute Mouse Configuration",
            Self::INTERFACE_NAME_INDEX => "Absolute Mouse Interface",
            _ => "weird index but ok",
        };
        StringDescriptor { string: s.to_string() }
    }
    fn device_qualifier_descriptor() -> DeviceQualifierDescriptor {
        DeviceQualifierDescriptor {
            usb_version: USB_VER_2_0,
            device_class: ClassCode(0),
            device_subclass: SubclassCode(0),
            device_protocol: ProtocolCode(0),
            max_packet_size_0: MaxSizeZeroEP::_64,
            num_configurations: 1,
        }
    }

    /// Defines an HID Report Descriptor with as many buttons and axes as make
    /// sense for the pointer data defined in the RFB RFC.
    // todoish: might be nice to make a nice builder-pattern thing someday, i.e.
    // RepDsc::new(Mouse).with_buttons(5).with_axes([X,Y], 0..=0x7fff, Absolute)
    // and also use as a source of truth for where in the payload to place data
    pub fn report_descriptor() -> ReportDescriptor {
        ReportDescriptor::new(
            HIDReportType::Input,
            vec![
                UsagePage::GenericDesktopControls.item(),
                GenericDesktopUsage::Mouse.item(),
                Collection::Application.items([
                    // ItemTag::ReportID.one_byte(1),
                    GenericDesktopUsage::Pointer.item(),
                    Collection::Physical.items([
                        UsagePage::Button.item(),
                        // HID Button values are, for a righty-mouse mapping,
                        // left, right, middle
                        ItemTag::UsageMinimum.one_byte(1),
                        ItemTag::UsageMaximum.one_byte(5),
                        ItemTag::LogicalMinimum.one_byte(0),
                        ItemTag::LogicalMaximum.one_byte(1),
                        // 5 buttons to report...
                        ItemTag::ReportCount.one_byte(5),
                        ItemTag::ReportSize.one_byte(1),
                        // 1 bit each
                        InputOutputFeatureItem(0)
                            .with_constant(false) // data
                            .with_variable(true) // variable
                            .with_relative(false) // absolute
                            .input(),
                        // 3 bit padding to round out the byte in the report
                        // (similar to Mouse example in HID 1.11 sect E.10)
                        ItemTag::ReportCount.one_byte(1),
                        ItemTag::ReportSize.one_byte(3),
                        InputOutputFeatureItem(0).with_constant(true).input(),
                        UsagePage::GenericDesktopControls.item(),
                        GenericDesktopUsage::X.item(),
                        GenericDesktopUsage::Y.item(),
                        ItemTag::LogicalMinimum.one_byte(0),
                        ItemTag::LogicalMaximum.two_byte(0x7fffu32),
                        // two axes, 16-bits each
                        ItemTag::ReportSize.one_byte(16),
                        ItemTag::ReportCount.one_byte(2),
                        InputOutputFeatureItem(0)
                            .with_constant(false) // data
                            .with_variable(true) // variable
                            .with_relative(false) // absolute
                            .input(),
                        // normal vertical scroll wheel
                        UsagePage::GenericDesktopControls.item(),
                        GenericDesktopUsage::Wheel.item(),
                        ItemTag::LogicalMinimum.one_byte(-127i8 as u32),
                        ItemTag::LogicalMaximum.one_byte(127),
                        ItemTag::ReportSize.one_byte(8),
                        ItemTag::ReportCount.one_byte(1),
                        InputOutputFeatureItem(0)
                            .with_constant(false) // data
                            .with_variable(true) // variable
                            .with_relative(true) // absolute
                            .input(),
                        // horizontal scroll wheel is a bit more obscure
                        UsagePage::Consumer.item(),
                        ConsumerUsage::ACPan.item(),
                        ItemTag::LogicalMinimum.one_byte(-127i8 as u32),
                        ItemTag::LogicalMaximum.one_byte(127),
                        ItemTag::ReportSize.one_byte(8),
                        ItemTag::ReportCount.one_byte(1),
                        InputOutputFeatureItem(0)
                            .with_constant(false) // data
                            .with_variable(true) // variable
                            .with_relative(true) // absolute
                            .input(),
                    ]),
                ]),
            ],
        )
    }

    fn payload_for(
        &self,
        req: ControlRequestInfo<HIDRequestInfo>,
    ) -> Result<Vec<u8>> {
        Ok(match req {
            ControlRequestInfo::GetDescriptor { descriptor_type, index } => {
                probes::usb_get_descriptor!(|| (descriptor_type as u8, index));
                // Box<> may incur an alloc, but this is called infrequently.
                // mostly kept so slog::trace!("{descriptor:?}") isn't duped
                let descriptor: Box<dyn Descriptor> = match descriptor_type {
                    DescriptorType::Device => {
                        Box::new(Self::device_descriptor())
                    }
                    DescriptorType::Configuration => {
                        Box::new(Self::config_descriptor())
                    }
                    DescriptorType::String => {
                        Box::new(Self::string_descriptor(index))
                    }
                    DescriptorType::DeviceQualifier => {
                        Box::new(Self::device_qualifier_descriptor())
                    }
                    DescriptorType::Report => {
                        Box::new(Self::report_descriptor())
                    }
                    x => return Err(Error::UnimplementedDescriptor(x)),
                };
                slog::trace!(self.log, "usb: GET_DESCRIPTOR({descriptor:?})");
                descriptor.serialize().collect()
            }
            ControlRequestInfo::GetStatus => {
                // USB 2.0 sect 9.4.5 - two-byte response where lowest-order
                // bits are 'self powered' and 'remote wakeup'
                let attrib = Self::config_descriptor().attributes;
                (attrib.self_powered() as u16
                    | (attrib.remote_wakeup() as u16 * 2))
                    .to_le_bytes()
                    .to_vec()
            }
            ControlRequestInfo::Class(HIDRequestInfo::GetIdle {
                report_id: _,
                interface: _,
            }) => {
                vec![self.idle_duration_4ms]
            }
            ControlRequestInfo::Class(HIDRequestInfo::GetReport {
                report_type: HIDReportType::Input,
                report_id: _,
                interface: _,
            }) => {
                let report = self.report.lock().unwrap();
                report.last_data.to_vec()
            }
            x => {
                return Err(Error::UnimplementedRequestBehavior(format!(
                    "{x:?}"
                )))
            }
        })
    }
}

impl UsbDevice for HIDTabletDevice {
    fn new_transfer_descriptor(&self) {
        self.port_hdl.reset_edtla();
    }

    fn stop_transfers_on_endpoint(
        &mut self,
        endpoint_id: EndpointId,
    ) -> Result<Option<TransferTrb>> {
        if let Ok(ep) = self.interrupt_ep_mut(endpoint_id) {
            Ok(ep.stop_endpoint())
        } else {
            Ok(None)
        }
    }

    fn setup_stage(
        &mut self,
        endpoint_id: EndpointId,
        setup: SetupData,
    ) -> Result<()> {
        if let Some(req) =
            self.control_ep_mut(endpoint_id)?.setup_stage(setup)?
        {
            let payload = self.payload_for(req)?;
            self.control_endpoint.as_mut().unwrap().set_payload(payload)?;
        }
        Ok(())
    }

    fn data_stage(
        &mut self,
        endpoint_id: EndpointId,
        trbs: Vec<TransferTrb>,
        data_direction: RequestDirection,
        memctx: &MemCtx,
    ) -> Result<()> {
        self.control_ep_mut(endpoint_id)?.data_stage(
            &trbs,
            data_direction,
            memctx,
        )
    }

    fn set_address(
        &mut self,
        slot_id: SlotId,
        _port_id: crate::hw::usb::xhci::port::PortId,
    ) {
        self.slot_id = Some(slot_id);
        self.control_endpoint = Some(ControlEndpoint::new(
            slot_id,
            EndpointId::from(1),
            Arc::clone(&self.port_hdl),
            &self.log,
        ));
    }

    fn configure_endpoint(
        &mut self,
        slot_id: SlotId,
        endpoint_id: EndpointId,
        ep_ctx: &EndpointContext,
    ) -> Result<()> {
        self.slot_id = Some(slot_id);
        if endpoint_id == self.ctrl_ep_id {
            if ep_ctx.endpoint_type() != EndpointType::Control {
                return Err(Error::InvalidEndpointType(
                    endpoint_id,
                    ep_ctx.endpoint_type(),
                ));
            }
            self.control_endpoint = Some(ControlEndpoint::new(
                slot_id,
                endpoint_id,
                Arc::clone(&self.port_hdl),
                &self.log,
            ));
            Ok(())
        } else if endpoint_id == self.intr_ep_id {
            if ep_ctx.endpoint_type() != EndpointType::InterruptIn {
                return Err(Error::InvalidEndpointType(
                    endpoint_id,
                    ep_ctx.endpoint_type(),
                ));
            }
            let interrupt_in_endpoint = InterruptInEndpoint::new(
                ep_ctx.interval_as_duration(),
                Arc::clone(&self.port_hdl),
                Arc::clone(&self.pci_state),
                self.guest_time.clone(),
                slot_id,
                endpoint_id,
                &self.log,
            );
            // reference for external access
            self.report
                .lock()
                .unwrap()
                .set_ep_data(interrupt_in_endpoint.data_ref());
            self.interrupt_endpoint = Some(interrupt_in_endpoint);
            Ok(())
        } else {
            Err(Error::InvalidEndpoint(endpoint_id))
        }
    }

    fn normal_transfer(
        &mut self,
        endpoint_id: EndpointId,
        xfer_trbs: Vec<TransferTrb>,
    ) -> Result<()> {
        match self.interrupt_ep_mut(endpoint_id) {
            Ok(ep) => {
                ep.normal_transfer(xfer_trbs);
            }
            Err(e) => {
                slog::warn!(self.log, "Guest tried to send Transfer TRBs to uninitialized {endpoint_id:?}: {e}");
                if let Err(e) = self.port_hdl.send_error_event(
                    xfer_trbs.first().map(|trb| trb.trb_pointer()),
                    TrbCompletionCode::EndpointNotEnabledError,
                    self.slot_id.unwrap_or(SlotId::from(0)),
                    endpoint_id,
                ) {
                    slog::error!(self.log, "xHC was unable to enqueue an Endpoint Not Enabled Error TRB on the Event Ring: {e}");
                }
            }
        }
        Ok(())
    }

    fn status_stage(
        &mut self,
        endpoint_id: EndpointId,
        status_direction: RequestDirection,
    ) -> Result<()> {
        match self
            .control_ep_mut(endpoint_id)?
            .status_stage(status_direction)?
        {
            Some(ControlEPStatusStageResult { request, payload: _ }) => {
                match request {
                    ControlRequestInfo::SetConfiguration { configuration } => {
                        if configuration == Self::CONFIGURATION_VALUE {
                            // device activates given configuration
                            Ok(())
                        } else if configuration.0 == 0 {
                            // device returns to 'addressed state',
                            // so no interrupt EP in this 'configuration'
                            self.interrupt_endpoint = None;
                            Ok(())
                        } else {
                            Err(Error::UnimplementedRequestBehavior(format!(
                                "{configuration:?} not in device descriptor"
                            )))
                        }
                    }
                    ControlRequestInfo::Class(HIDRequestInfo::SetIdle {
                        duration_4ms,
                        report_id: _,
                        interface: _,
                    }) => {
                        if duration_4ms == 0 {
                            // yes, slightly silly in an if==0, but models the spec
                            self.idle_duration_4ms = duration_4ms;
                            Ok(())
                        } else {
                            // error if not 0 (we don't implement anything else)
                            Err(Error::UnimplementedRequestBehavior(format!(
                                "Nonzero duration in HID SET_IDLE request: {duration_4ms}"
                            )))
                        }
                    }
                    x => Err(Error::UnimplementedRequestBehavior(format!(
                        "{x:?}"
                    ))),
                }
            }
            None => Ok(()),
        }
    }

    fn import(
        &mut self,
        value: &super::migrate::UsbDeviceV1,
    ) -> core::result::Result<(), crate::migrate::MigrateStateError> {
        let super::migrate::UsbDeviceV1 { device_type, endpoints } = value;
        let super::migrate::UsbDeviceTypeV1::Tablet(tablet_data) = device_type
        else {
            return Err(crate::migrate::MigrateStateError::ImportFailed(
                format!("USB device type mismatch {device_type:?} != Tablet"),
            ));
        };

        self.idle_duration_4ms = tablet_data.idle_duration_4ms;
        self.report.lock().unwrap().last_data = tablet_data.report_data;

        if let Some(ep_payload) = endpoints.get(&1) {
            let super::endpoint::migrate::EndpointV1::Control(ctrl_ep_payload) =
                ep_payload
            else {
                return Err(crate::migrate::MigrateStateError::ImportFailed(
                    "Wrong endpoint type for USB Default Control Endpoint"
                        .to_string(),
                ));
            };
            if let Some(ctrl_ep) = self.control_endpoint.as_mut() {
                ctrl_ep.import(ctrl_ep_payload);
            } else {
                self.control_endpoint = Some(ControlEndpoint::new_migrated(
                    ctrl_ep_payload,
                    Arc::clone(&self.port_hdl),
                    &self.log,
                ));
            }
        } else {
            self.control_endpoint = None;
        }
        if let Some(ep_payload) = endpoints.get(&3) {
            let super::endpoint::migrate::EndpointV1::InterruptIn(
                intr_in_ep_payload,
            ) = ep_payload
            else {
                return Err(crate::migrate::MigrateStateError::ImportFailed(
                    "wrong endpoint type for USB Interrupt IN Endpoint"
                        .to_string(),
                ));
            };
            if let Some(intr_ep) = self.interrupt_endpoint.as_mut() {
                intr_ep.import(intr_in_ep_payload)?;
            } else {
                let interrupt_in_endpoint = InterruptInEndpoint::new_migrated(
                    intr_in_ep_payload,
                    Arc::clone(&self.port_hdl),
                    Arc::clone(&self.pci_state),
                    self.guest_time.clone(),
                    &self.log,
                )?;
                self.report
                    .lock()
                    .unwrap()
                    .set_ep_data(interrupt_in_endpoint.data_ref());
                self.interrupt_endpoint = Some(interrupt_in_endpoint);
            }
        } else {
            self.interrupt_endpoint = None;

            return Err(crate::migrate::MigrateStateError::ImportFailed(
                "USB interrupt endpoint 1 missing".to_string(),
            ));
        }
        Ok(())
    }

    fn export(
        &self,
    ) -> core::result::Result<
        super::migrate::UsbDeviceV1,
        crate::migrate::MigrateStateError,
    > {
        let mut endpoints: BTreeMap<_, _> = self
            .control_endpoint
            .as_ref()
            .map(|ctrl_ep| (1, ctrl_ep.export()))
            .into_iter()
            .collect();
        if let Some(interrupt_ep) = &self.interrupt_endpoint {
            endpoints.insert(3, interrupt_ep.export()?);
        }
        Ok(super::migrate::UsbDeviceV1 {
            device_type: super::migrate::UsbDeviceTypeV1::Tablet(
                TabletDeviceV1 {
                    report_data: self.report.lock().unwrap().last_data,
                    idle_duration_4ms: self.idle_duration_4ms,
                },
            ),
            endpoints,
        })
    }
}

pub mod migrate {
    use super::REPORT_SIZE;
    use serde::{Deserialize, Serialize};

    #[derive(Serialize, Deserialize, Debug)]
    pub struct TabletDeviceV1 {
        pub report_data: [u8; REPORT_SIZE],
        pub idle_duration_4ms: u8,
    }
}

#[cfg(test)]
mod test {
    use std::sync::{Arc, Mutex};

    use rfb::proto::{MouseButtons, PointerEvent, Position};
    use zerocopy::FromZeros;

    use crate::common::GuestAddr;
    use crate::hw::usb::test::pci_state_for_test;
    use crate::hw::usb::usbdev::descriptor::Descriptor;
    use crate::hw::usb::usbdev::hid::{HIDReportType, HIDRequest};
    use crate::hw::usb::usbdev::requests::{
        RequestDirection, RequestRecipient, RequestType, SetupData,
    };
    use crate::hw::usb::usbdev::vnc_tablet::{
        HIDTabletDevice, HIDTabletReport, REPORT_SIZE,
    };
    use crate::hw::usb::usbdev::UsbDevice;
    use crate::hw::usb::xhci::bits::device_context::{
        EndpointContext, EndpointType,
    };
    use crate::hw::usb::xhci::bits::ring_data::{
        Trb, TrbControlField, TrbControlFieldDataStage, TrbControlFieldNormal,
        TrbStatusField, TrbStatusFieldTransfer, TrbType,
    };
    use crate::hw::usb::xhci::controller::XhciPortHandle;
    use crate::hw::usb::xhci::device_slots::{EndpointId, SlotId};
    use crate::hw::usb::xhci::interrupter::EventSender;
    use crate::hw::usb::xhci::rings::consumer::transfer::TransferTrb;
    use crate::vmm::time::VmGuestTime;

    #[rustfmt::skip] // hardcoded array with specific whitespace for legibility
    #[test]
    // dual purpose: verify that ReportDescriptor::serialize() does what we want
    // and trips on attempts to change the report format for this device in
    // particular - which we must not do since we care about live migration!
    fn tablet_descriptor_serialization() {
        let serialized: Vec<u8> =
            super::HIDTabletDevice::report_descriptor().serialize().collect();
        // similar to HID 1.11 sect E.10, but absolute x/y, and with scroll wheels
        assert_eq!(serialized.as_slice(), &[
          5, 1, // usage page (generic desktop)
          9, 2, // usage (mouse)
          0xA1, 1, // collection (application)
            9, 1, // usage (pointer)
            0xA1, 0, // collection (physical)
              5, 9, // usage page (buttons)
                0x19, 1, // usage minimum (1)
                0x29, 5, // usage maximum (5)
                0x15, 0, // logical minimum (0)
                0x25, 1, // logical maximum (1)
                0x95, 5, // report count (5)
                0x75, 1, // report size (1)
                0x81, 2, // input (data, variable, absolute), 5 button bits
                0x95, 1, // report count (1)
                0x75, 3, // report size (3)
                0x81, 1, // input (constant), 3 bit padding
              5, 1, // usage page (generic desktop)
                9, 0x30, // usage (x)
                9, 0x31, // usage (y)
                0x15, 0, // logical minimum (0)
                0x26, 0xff, 0x7f, // logical maximum (0x7fff)
                0x75, 0x10, // report size (16)
                0x95, 2, // report count (2)
                0x81, 2, // input (data, var, abs), 2 position shorts (x & y)
              5, 1, // usage page (generic desktop)
                9, 0x38, // usage (wheel)
                0x15, 0x81, // logical minimum (-127)
                0x25, 0x7f, // logical maximum (127)
                0x75, 8, // report size (8)
                0x95, 1, // report count (1)
                0x81, 6, // input (data, variable, relative)
              5, 0x0C, // usage page (consumer devices)
                0x0A, 0x38, 0x02, // usage (application controls: pan)
                0x15, 0x81, // logical minimum (-127)
                0x25, 0x7f, // logical maximum (127)
                0x75, 8, // report size (8)
                0x95, 1, // report count (1)
                0x81, 6, // input (data, variable, relative)
            0xC0, // end collection
          0xC0, // end collection
        ])
    }

    #[test]
    fn tablet_pointer_events() {
        let log = slog::Logger::root(slog::Discard, slog::o!());
        let pci_state = pci_state_for_test();
        let event_sender = Arc::new(EventSender::new(&pci_state));
        let port_hdl = Arc::new(XhciPortHandle::new_test(event_sender.clone()));
        let guest_time = VmGuestTime::new_test();
        let memctx = pci_state.acc_mem.access().unwrap();
        // dimensions of 'screen' are ridiculous-looking to make the coords-to-
        // HID-axis-range conversion be 1:1 such that raw reports are legible
        let spec =
            rgb_frame::Spec::new(0x7fff, 0x7fff, rgb_frame::FourCC::BA24);

        let mut ep_ctx = EndpointContext::new_zeroed();

        let slot_id = SlotId::try_from(2).unwrap();
        let intr_ep_id = EndpointId::try_from(3).unwrap();
        let ctrl_ep_id = EndpointId::try_from(1).unwrap();
        const REPORT_DEST_ADDR: GuestAddr = GuestAddr(1024);

        let report: Arc<Mutex<HIDTabletReport>> = Arc::default();

        let mut tablet = HIDTabletDevice::new(
            report.clone(),
            port_hdl,
            pci_state.clone(),
            guest_time,
            log,
        );

        // endpoint not enabled, no transfer data to populate
        assert!(report.lock().unwrap().xfer_dataref.is_none());

        // HID-class-specific request on control endpoint should work though,
        // so we should be able to send an event 'from VNC' and have it be
        // retrievable by such
        report.lock().unwrap().pointer_event(
            PointerEvent {
                position: Position { x: 4, y: 5 },
                pressed: MouseButtons::LEFT,
            },
            spec,
        );

        // (and here's part of why nothing uses this method in practice, vs.
        // sending one Normal TRB at a time on the intr-in endpoint)
        ep_ctx.set_endpoint_type(EndpointType::Control);
        tablet.configure_endpoint(slot_id, ctrl_ep_id, &ep_ctx).unwrap();
        tablet
            .setup_stage(
                ctrl_ep_id,
                SetupData(0)
                    .with_request_type(RequestType::Class)
                    .with_recipient(RequestRecipient::Device)
                    .with_direction(RequestDirection::DeviceToHost)
                    .with_length(REPORT_SIZE as u16)
                    .with_request(HIDRequest::GetReport as u8)
                    .with_value(u16::from_be_bytes([
                        HIDReportType::Input as u8,
                        0,
                    ])),
            )
            .unwrap();
        tablet
            .data_stage(
                ctrl_ep_id,
                vec![TransferTrb::new(
                    &Trb {
                        parameter: REPORT_DEST_ADDR.0,
                        status: TrbStatusField {
                            transfer: TrbStatusFieldTransfer(0)
                                .with_trb_transfer_length(REPORT_SIZE as u32),
                        },
                        control: TrbControlField {
                            data_stage: TrbControlFieldDataStage(0)
                                .with_trb_type(TrbType::DataStage)
                                .with_interrupt_on_completion(true),
                        },
                    },
                    &GuestAddr(0),
                    None,
                )
                .unwrap()],
                RequestDirection::DeviceToHost,
                &memctx,
            )
            .unwrap();
        tablet
            .status_stage(ctrl_ep_id, RequestDirection::HostToDevice)
            .unwrap();

        assert_eq!(
            *memctx.read::<[u8; REPORT_SIZE]>(REPORT_DEST_ADDR).unwrap(),
            [1, 4, 0, 5, 0, 0, 0]
        );

        // configure the interrupt-in endpoint for 'normal' operation..
        ep_ctx.set_endpoint_type(EndpointType::InterruptIn);
        tablet.configure_endpoint(slot_id, intr_ep_id, &ep_ctx).unwrap();

        assert!(memctx.write(REPORT_DEST_ADDR, &[0; REPORT_SIZE]));
        tablet
            .normal_transfer(
                intr_ep_id,
                vec![TransferTrb::new(
                    &Trb {
                        parameter: REPORT_DEST_ADDR.0,
                        status: TrbStatusField {
                            transfer: TrbStatusFieldTransfer(0)
                                .with_trb_transfer_length(REPORT_SIZE as u32),
                        },
                        control: TrbControlField {
                            normal: TrbControlFieldNormal(0)
                                .with_trb_type(TrbType::Normal)
                                .with_interrupt_on_completion(true),
                        },
                    },
                    &GuestAddr(0),
                    None,
                )
                .unwrap()],
            )
            .unwrap();
        // no change until we send another event...
        assert_eq!(
            *memctx.read::<[u8; REPORT_SIZE]>(REPORT_DEST_ADDR).unwrap(),
            [0; REPORT_SIZE]
        );

        report.lock().unwrap().pointer_event(
            PointerEvent {
                position: Position { x: 6, y: 7 },
                pressed: MouseButtons::empty(),
            },
            spec,
        );

        assert_eq!(
            *memctx.read::<[u8; REPORT_SIZE]>(REPORT_DEST_ADDR).unwrap(),
            [0, 6, 0, 7, 0, 0, 0]
        );
    }
}
