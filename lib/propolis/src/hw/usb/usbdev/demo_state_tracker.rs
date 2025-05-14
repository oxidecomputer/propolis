// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::sync::Arc;

use crate::hw::ids::usb::{PROPOLIS_USB_NULL_DEV_ID, VENDOR_OXIDE};
use crate::hw::usb::usbdev::endpoint::control::ControlEPStatusStageResult;
use crate::hw::usb::xhci::controller::XhciPortHandle;
use crate::hw::usb::xhci::device_slots::{EndpointId, SlotId};
use crate::hw::usb::xhci::rings::consumer::transfer::TransferTrb;
use crate::vmm::MemCtx;

use super::descriptor::*;
use super::endpoint::control::{
    ControlEndpoint, ControlRequestInfo, NoClassRequestInfo,
};
use super::probes;
use super::requests::{RequestDirection, SetupData};
use super::{Error, Result, UsbDevice};

/// This is a trivial device purely for testing the USB+xHCI implementation.
/// It implements the minimal feature set (i.e. answering GET_DESCRIPTOR)
/// to appear valid in the guest OS.
/// Its descriptors define a single configuration and interface, containing
/// only the default control endpoint.
pub struct NullUsbDevice {
    control_endpoint: Option<ControlEndpoint<NoClassRequestInfo>>,
    port_hdl: Arc<XhciPortHandle>,
    log: slog::Logger,
}

impl UsbDevice for NullUsbDevice {
    fn new_transfer_descriptor(&self) {
        self.port_hdl.reset_edtla();
    }

    // no transfers handled out-of-band
    fn stop_transfers_on_endpoint(
        &mut self,
        _endpoint_id: EndpointId,
    ) -> Result<Option<TransferTrb>> {
        Ok(None)
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
                    ControlRequestInfo::SetConfiguration { .. } => Ok(()),
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
        let super::migrate::UsbDeviceTypeV1::Null = device_type else {
            return Err(crate::migrate::MigrateStateError::ImportFailed(
                format!("USB device type mismatch {device_type:?} != Null"),
            ));
        };
        let Some(super::endpoint::migrate::EndpointV1::Control(ep)) =
            endpoints.get(&0)
        else {
            return Err(crate::migrate::MigrateStateError::ImportFailed(
                "USB Default Control Endpoint missing from payload".to_string(),
            ));
        };

        self.control_endpoint = Some(ControlEndpoint::new_migrated(
            ep,
            Arc::clone(&self.port_hdl),
            &self.log,
        ));
        Ok(())
    }

    fn export(
        &self,
    ) -> core::result::Result<
        super::migrate::UsbDeviceV1,
        crate::migrate::MigrateStateError,
    > {
        Ok(super::migrate::UsbDeviceV1 {
            device_type: super::migrate::UsbDeviceTypeV1::Null,
            endpoints: self
                .control_endpoint
                .as_ref()
                .map(|ep| (0, ep.export()))
                .into_iter()
                .collect(),
        })
    }
    fn set_address(
        &mut self,
        slot_id: SlotId,
        _port_id: crate::hw::usb::xhci::port::PortId,
    ) {
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
        _ep_ctx: &crate::hw::usb::xhci::bits::device_context::EndpointContext,
    ) -> Result<()> {
        if u8::from(endpoint_id) == 1 {
            self.control_endpoint = Some(ControlEndpoint::new(
                slot_id,
                endpoint_id,
                Arc::clone(&self.port_hdl),
                &self.log,
            ));
            Ok(())
        } else {
            Err(Error::InvalidEndpoint(endpoint_id))
        }
    }

    fn normal_transfer(
        &mut self,
        _endpoint_id: EndpointId,
        _normal_td: Vec<TransferTrb>,
    ) -> Result<()> {
        Ok(())
    }
}

impl NullUsbDevice {
    const MANUFACTURER_NAME_INDEX: StringIndex = StringIndex(1);
    const PRODUCT_NAME_INDEX: StringIndex = StringIndex(2);
    const SERIAL_INDEX: StringIndex = StringIndex(3);
    const CONFIG_NAME_INDEX: StringIndex = StringIndex(4);
    const INTERFACE_NAME_INDEX: StringIndex = StringIndex(5);

    pub fn new(port_hdl: Arc<XhciPortHandle>, log: slog::Logger) -> Self {
        Self { control_endpoint: None, port_hdl, log }
    }

    fn control_ep_mut(
        &mut self,
        endpoint_id: EndpointId,
    ) -> Result<&mut ControlEndpoint<NoClassRequestInfo>> {
        if u8::from(endpoint_id) != 1 {
            return Err(Error::InvalidEndpoint(endpoint_id));
        }
        self.control_endpoint
            .as_mut()
            .ok_or(Error::EndpointNotConfigured(endpoint_id))
    }

    fn device_descriptor() -> DeviceDescriptor {
        DeviceDescriptor {
            usb_version: USB_VER_2_0,
            device_class: ClassCode(0),
            device_subclass: SubclassCode(0),
            device_protocol: ProtocolCode(0),
            max_packet_size_0: MaxSizeZeroEP::_64,
            vendor_id: VendorId(VENDOR_OXIDE),
            product_id: ProductId(PROPOLIS_USB_NULL_DEV_ID),
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
            config_value: ConfigurationValue(0),
            configuration_name: Self::CONFIG_NAME_INDEX,
            attributes: ConfigurationAttributes::default(),
            specific_augmentations: vec![],
        }
    }
    fn interface_descriptor() -> InterfaceDescriptor {
        InterfaceDescriptor {
            interface_num: 0,
            alternate_setting: 0,
            endpoints: vec![Self::endpoint_descriptor()],
            class: InterfaceClass(0),
            subclass: InterfaceSubclass(0),
            protocol: InterfaceProtocol(0),
            interface_name: Self::INTERFACE_NAME_INDEX,
            specific_augmentations: vec![],
        }
    }
    fn endpoint_descriptor() -> EndpointDescriptor {
        EndpointDescriptor {
            endpoint_addr: 1,
            direction: None,
            attributes: EndpointAttributes::default(),
            max_packet_size: 64,
            interval: 1,
            specific_augmentations: vec![],
        }
    }
    fn string_descriptor(idx: u8) -> StringDescriptor {
        let s: &str = match StringIndex(idx) {
            Self::MANUFACTURER_NAME_INDEX => "Oxide Computer Company",
            Self::PRODUCT_NAME_INDEX => "Generic USB 2.0 Encabulator",
            Self::SERIAL_INDEX => "9001",
            Self::CONFIG_NAME_INDEX => "Configuration",
            Self::INTERFACE_NAME_INDEX => "Interface",
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
            num_configurations: 0,
        }
    }
    fn payload_for(
        &self,
        req: ControlRequestInfo<NoClassRequestInfo>,
    ) -> Result<Vec<u8>> {
        Ok(match req {
            ControlRequestInfo::GetDescriptor { descriptor_type, index } => {
                probes::usb_get_descriptor!(|| (descriptor_type as u8, index));
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
                    x => return Err(Error::UnimplementedDescriptor(x)),
                };
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
            x => {
                return Err(Error::UnimplementedRequestBehavior(format!(
                    "{x:?}"
                )))
            }
        })
    }
}
