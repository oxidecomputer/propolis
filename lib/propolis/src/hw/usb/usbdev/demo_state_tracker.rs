// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use crate::{
    common::{GuestData, GuestRegion},
    hw::usb::xhci::rings::consumer::transfer::PointerOrImmediate,
    vmm::MemCtx,
};

use super::{
    descriptor::*,
    probes,
    requests::{Request, RequestDirection, SetupData, StandardRequest},
    {Error, Result},
};

/// This is a hard-coded faux-device that purely exists to test the xHCI implementation.
#[derive(Default)]
pub struct NullUsbDevice {
    current_setup: Option<SetupData>,
    payload: Vec<u8>,
    bytes_transferred: usize,
}

impl NullUsbDevice {
    const MANUFACTURER_NAME_INDEX: StringIndex = StringIndex(0);
    const PRODUCT_NAME_INDEX: StringIndex = StringIndex(1);
    const SERIAL_INDEX: StringIndex = StringIndex(2);
    const CONFIG_NAME_INDEX: StringIndex = StringIndex(3);
    const INTERFACE_NAME_INDEX: StringIndex = StringIndex(4);

    fn device_descriptor() -> DeviceDescriptor {
        DeviceDescriptor {
            usb_version: USB_VER_2_0,
            device_class: ClassCode(0),
            device_subclass: SubclassCode(0),
            device_protocol: ProtocolCode(0),
            max_packet_size_0: MaxSizeZeroEP::_64,
            vendor_id: VendorId(0x1de),
            product_id: ProductId(0xdead),
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
            endpoint_addr: 0,
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
            Self::CONFIG_NAME_INDEX => "MyCoolConfiguration",
            Self::INTERFACE_NAME_INDEX => "MyNotQuiteAsCoolInterface",
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

    pub fn setup_stage(&mut self, req: SetupData) -> Result<()> {
        match req.direction() {
            RequestDirection::DeviceToHost => {
                self.payload = self.payload_for(&req)?;
            }
            RequestDirection::HostToDevice => {
                self.payload.clear();
            }
        }
        self.bytes_transferred = 0;
        self.current_setup = Some(req);
        Ok(())
    }

    pub fn data_stage(
        &mut self,
        data_buffer: PointerOrImmediate,
        data_direction: RequestDirection,
        memctx: &MemCtx,
    ) -> Result<usize> {
        if let Some(setup_data) = self.current_setup.as_ref() {
            if data_direction != setup_data.direction() {
                return Err(Error::SetupVsDataDirectionMismatch(
                    setup_data.direction(),
                    data_direction,
                ));
            }
            let count = match setup_data.direction() {
                RequestDirection::DeviceToHost => {
                    let PointerOrImmediate::Pointer(region) = data_buffer
                    else {
                        return Err(Error::ImmediateParameterForOutDataStage);
                    };
                    memctx
                        .write_from(
                            region.0,
                            &self.payload[self.bytes_transferred..],
                            region.1,
                        )
                        .ok_or(Error::DataStageWriteFailed)?
                }
                RequestDirection::HostToDevice => match data_buffer {
                    PointerOrImmediate::Pointer(GuestRegion(ptr, len)) => {
                        self.payload.resize(self.bytes_transferred + len, 0u8);
                        memctx
                            .read_into(
                                ptr,
                                &mut GuestData::from(
                                    &mut self.payload[self.bytes_transferred..],
                                ),
                                len,
                            )
                            .ok_or(Error::DataStageReadFailed)?
                    }
                    PointerOrImmediate::Immediate(arr, len) => {
                        self.payload.extend_from_slice(&arr[..len]);
                        len
                    }
                },
            };
            self.bytes_transferred += count;
            Ok(count)
        } else {
            Err(Error::NoSetupStageBefore("Data Stage"))
        }
    }

    pub fn status_stage(
        &mut self,
        status_direction: RequestDirection,
    ) -> Result<()> {
        if let Some(setup) = self.current_setup.take() {
            if status_direction == setup.direction() {
                return Err(Error::SetupVsStatusDirectionMatch(
                    status_direction,
                ));
            }

            let result = match setup.direction() {
                RequestDirection::HostToDevice => match setup.request() {
                    Request::Standard(StandardRequest::SetConfiguration) => {
                        self.set_configuration()
                    }
                    x => Err(Error::UnimplementedRequest(x)),
                },
                RequestDirection::DeviceToHost => Ok(()),
            };

            self.payload.clear();
            self.bytes_transferred = 0;
            result
        } else {
            Err(Error::NoSetupStageBefore("Status Stage"))
        }
    }

    fn set_configuration(&mut self) -> Result<()> {
        if self.payload.is_empty() {
            Ok(())
        } else {
            Err(Error::InvalidPayloadForRequest(
                Request::Standard(StandardRequest::SetConfiguration),
                self.payload.clone(),
            ))
        }
    }

    fn payload_for(&self, setup_data: &SetupData) -> Result<Vec<u8>> {
        match setup_data.request() {
            Request::Standard(StandardRequest::GetDescriptor) => {
                let [desc, idx] = setup_data.value().to_be_bytes();
                let descriptor: Box<dyn Descriptor> =
                    match DescriptorType::from_repr(desc) {
                        Some(DescriptorType::Device) => {
                            Box::new(Self::device_descriptor())
                        }
                        Some(DescriptorType::Configuration) => {
                            Box::new(Self::config_descriptor())
                        }
                        Some(DescriptorType::String) => {
                            Box::new(Self::string_descriptor(idx))
                        }
                        Some(DescriptorType::DeviceQualifier) => {
                            Box::new(Self::device_qualifier_descriptor())
                        }
                        Some(x) => {
                            return Err(Error::UnimplementedDescriptor(x))
                        }
                        None => return Err(Error::UnknownDescriptorType(desc)),
                    };
                probes::usb_get_descriptor!(|| (desc, idx));
                // slog::debug!(log, "usb: GET_DESCRIPTOR({descriptor:?})");
                Ok(descriptor.serialize().collect())
            }
            Request::Standard(StandardRequest::GetStatus) => {
                // USB 2.0 sect 9.4.5 - two-byte response where lowest-order
                // bits are 'self powered' and 'remote wakeup'
                let attrib = Self::config_descriptor().attributes;
                Ok((attrib.self_powered() as u16
                    | (attrib.remote_wakeup() as u16 * 2))
                    .to_le_bytes()
                    .to_vec())
            }
            x => Err(Error::UnimplementedRequest(x)),
        }
    }

    pub fn import(
        &mut self,
        value: &super::migrate::UsbDeviceV1,
    ) -> core::result::Result<(), crate::migrate::MigrateStateError> {
        let super::migrate::UsbDeviceV1 { device_type, current_setup } = value;
        if *device_type != super::migrate::UsbDeviceTypeV1::Null {
            return Err(crate::migrate::MigrateStateError::ImportFailed(
                format!("USB device type mismatch {device_type:?} != Null"),
            ));
        }
        self.current_setup = current_setup.map(|x| SetupData(x));
        Ok(())
    }

    pub fn export(&self) -> super::migrate::UsbDeviceV1 {
        super::migrate::UsbDeviceV1 {
            device_type: super::migrate::UsbDeviceTypeV1::Null,
            current_setup: self.current_setup.as_ref().map(|x| x.0),
        }
    }
}
