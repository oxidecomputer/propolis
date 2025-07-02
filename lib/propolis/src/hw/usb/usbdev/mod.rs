// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

pub mod descriptor;
pub mod requests;

pub mod demo_state_tracker {
    use crate::{common::GuestRegion, vmm::MemCtx};

    use super::{
        descriptor::*,
        requests::{Request, RequestDirection, SetupData, StandardRequest},
    };

    /// This is a hard-coded faux-device that purely exists to test the xHCI implementation.
    #[derive(Default)]
    pub struct NullUsbDevice {
        current_setup: Option<SetupData>,
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

        pub fn set_request(&mut self, req: SetupData) -> Option<SetupData> {
            self.current_setup.replace(req)
        }

        pub fn data_stage(
            &mut self,
            region: GuestRegion,
            memctx: &MemCtx,
            log: &slog::Logger,
        ) -> Result<usize, &'static str> {
            if let Some(setup_data) = self.current_setup.as_ref() {
                match setup_data.direction() {
                    RequestDirection::DeviceToHost => {
                        let mut payload = vec![0u8; region.1];
                        let count =
                            self.payload_for(setup_data, &mut payload, log);
                        memctx.write_many(region.0, &payload[..count]);
                        Ok(count)
                    }
                    RequestDirection::HostToDevice => {
                        Err("host-to-device unimplemented")
                    }
                }
            } else {
                Err("no setup data")
            }
        }

        fn payload_for(
            &self,
            setup_data: &SetupData,
            dest_buf: &mut [u8],
            log: &slog::Logger,
        ) -> usize {
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
                                slog::error!(
                                    log,
                                    "usb: unimplemented descriptor: GetDescriptor({x:?})"
                                );
                                return 0;
                            }
                            None => {
                                slog::error!(
                                    log,
                                    "usb: unknown descriptor type: GetDescriptor({desc:#x})"
                                );
                                return 0;
                            }
                        };
                    slog::debug!(log, "usb: GET_DESCRIPTOR({descriptor:?})");
                    descriptor
                        .serialize()
                        .zip(dest_buf.iter_mut())
                        .map(|(src, dest)| *dest = src)
                        .count()
                }
                Request::Standard(x) => {
                    slog::error!(
                        log,
                        "usb: unimplemented request: Standard({x:?})"
                    );
                    return 0;
                }
                Request::Other(x) => {
                    slog::error!(
                        log,
                        "usb: unimplementd request: Other({x:#x})"
                    );
                    return 0;
                }
            }
        }

        pub fn import(
            &mut self,
            value: &super::migrate::UsbDeviceV1,
        ) -> Result<(), crate::migrate::MigrateStateError> {
            let super::migrate::UsbDeviceV1 { device_type, current_setup } =
                value;
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
}

pub mod migrate {
    use serde::{Deserialize, Serialize};

    #[derive(Serialize, Deserialize, Debug, PartialEq, Eq)]
    pub enum UsbDeviceTypeV1 {
        Null,
    }

    #[derive(Serialize, Deserialize)]
    pub struct UsbDeviceV1 {
        pub device_type: UsbDeviceTypeV1,
        pub current_setup: Option<u64>,
    }
}
