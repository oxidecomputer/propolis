// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use bitstruct::bitstruct;
use strum::FromRepr;

#[repr(transparent)]
pub struct Bcd16(pub u16);
impl core::fmt::Debug for Bcd16 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Bcd16({:#x})", self.0)
    }
}

pub const USB_VER_2_0: Bcd16 = Bcd16(0x200);

#[repr(transparent)]
#[derive(Debug)]
pub struct ClassCode(pub u8);
#[repr(transparent)]
#[derive(Debug)]
pub struct SubclassCode(pub u8);
#[repr(transparent)]
#[derive(Debug)]
pub struct ProtocolCode(pub u8);

#[repr(u8)]
#[derive(Copy, Clone, Debug)]
pub enum MaxSizeZeroEP {
    _8 = 8,
    _16 = 16,
    _32 = 32,
    _64 = 64,
}

#[repr(transparent)]
#[derive(Debug)]
pub struct VendorId(pub u16);
#[repr(transparent)]
#[derive(Debug)]
pub struct ProductId(pub u16);
#[repr(transparent)]
#[derive(Debug, PartialEq)]
pub struct StringIndex(pub u8);
#[repr(transparent)]
#[derive(Debug)]
pub struct ConfigurationValue(pub u8);

bitstruct! {
    #[derive(Debug)]
    pub struct ConfigurationAttributes(pub u8) {
        reserved: u8 = 0..5;
        pub remote_wakeup: bool = 5;
        pub self_powered: bool = 6;
        /// Reserved, but set to 1
        pub one: bool = 7;
    }
}
impl Default for ConfigurationAttributes {
    fn default() -> Self {
        Self(0).with_one(true)
    }
}

bitstruct! {
    #[derive(Default, Debug)]
    pub struct EndpointAttributes(pub u8) {
        /// control, isoch, bulk, interrupt. TODO: enum
        pub transfer_type: u8 = 0..2;
        pub isoch_synch_type: u8 = 2..4;
        pub isoch_usage_type: u8 = 4..6;
        reserved: u8 = 6..8;
    }
}

/// USB 2.0 table 9-5
#[repr(u8)]
#[derive(FromRepr, Debug)]
pub enum DescriptorType {
    Device = 1,
    Configuration = 2,
    String = 3,
    Interface = 4,
    Endpoint = 5,
    DeviceQualifier = 6,
    OtherSpeedConfiguration = 7,
    InterfacePower = 8,
    // OTG and Embedded Host Supplement v1.1a
    OnTheGo = 9,
    Debug = 10,
    InterfaceAssociation = 11,
    HID = 33,
}

#[repr(u8)]
#[derive(FromRepr, Debug)]
pub enum CountryCode {
    // TODO
    International = 13,
    UnitedStates = 33,
}

#[derive(Debug, Copy, Clone)]
pub enum LanguageId {
    Known(KnownLanguageId),
    Other(u16),
}
impl From<&LanguageId> for u16 {
    fn from(value: &LanguageId) -> Self {
        match value {
            LanguageId::Known(langid) => *langid as u16,
            LanguageId::Other(x) => *x,
        }
    }
}
impl From<&u16> for LanguageId {
    fn from(value: &u16) -> Self {
        KnownLanguageId::from_repr(*value)
            .map(Self::Known)
            .unwrap_or(Self::Other(*value))
    }
}

#[repr(u16)]
#[derive(FromRepr, Copy, Clone, Debug)]
pub enum KnownLanguageId {
    EnglishUS = 0x0409,
    HIDUsageDataDescriptor = 0x04ff,
    HIDVendorDefined1 = 0xf0ff,
    HIDVendorDefined2 = 0xf4ff,
    HIDVendorDefined3 = 0xf8ff,
    HIDVendorDefined4 = 0xfcff,
}

#[repr(transparent)]
#[derive(Debug)]
pub struct InterfaceClass(pub u8);
#[repr(transparent)]
#[derive(Debug)]
pub struct InterfaceSubclass(pub u8);
#[repr(transparent)]
#[derive(Debug)]
pub struct InterfaceProtocol(pub u8);

pub trait Descriptor: core::fmt::Debug {
    /// bLength. Size of serialized descriptor in bytes.
    fn length(&self) -> u8;

    fn descriptor_type(&self) -> DescriptorType;

    fn header(&self) -> [u8; 2] {
        [self.length(), self.descriptor_type() as u8]
    }

    fn serialize(&self) -> Box<dyn Iterator<Item = u8> + '_>;
}

/// Used in Configuration Descriptor's computation of wTotalLength to give
/// the size of all the descriptors provided when GET_DESCRIPTOR(Configuration)
/// is requested, and to follow Configuration Descriptor's own serialization
/// with their own payloads.
pub trait NestedDescriptor: Descriptor {
    fn total_length(&self) -> u16;
    fn serialize_all(&self) -> Box<dyn Iterator<Item = u8> + '_>;
}

/// Device Descriptor.
#[derive(Debug)]
pub struct DeviceDescriptor {
    /// bcdUSB. USB version in binary-coded decimal.
    pub usb_version: Bcd16,
    /// bDeviceClass.
    pub device_class: ClassCode,
    /// bDeviceSubClass.
    pub device_subclass: SubclassCode,
    /// bDeviceProtocol.
    pub device_protocol: ProtocolCode,
    /// bMaxPacketSize0.
    pub max_packet_size_0: MaxSizeZeroEP,
    /// idVendor.
    pub vendor_id: VendorId,
    /// idProduct.
    pub product_id: ProductId,
    /// bcdDevice.
    pub device_version: Bcd16,
    /// iManufacturer.
    pub manufacturer_name: StringIndex,
    /// iProduct.
    pub product_name: StringIndex,
    /// iSerial.
    pub serial: StringIndex,

    /// bNumConfigurations (u8) is the length of:
    pub configurations: Vec<ConfigurationDescriptor>,

    /// Descriptors of class-specific or vendor-specific augmentations.
    pub specific_augmentations: Vec<AugmentedDescriptor>,
}

impl Descriptor for DeviceDescriptor {
    /// bLength is 18 for Device Descriptor.
    fn length(&self) -> u8 {
        18
    }

    /// bDescriptorType is 1 for Device Descriptor.
    fn descriptor_type(&self) -> DescriptorType {
        DescriptorType::Device
    }

    /// USB 2.0 table 9-8
    fn serialize(&self) -> Box<dyn Iterator<Item = u8> + '_> {
        Box::new(
            self.header()
                .into_iter() // 0, 1
                .chain(self.usb_version.0.to_le_bytes()) // 2-3
                .chain([
                    self.device_class.0,          // 4
                    self.device_subclass.0,       // 5
                    self.device_protocol.0,       // 6
                    self.max_packet_size_0 as u8, // 7
                ])
                .chain(self.vendor_id.0.to_le_bytes()) // 8-9
                .chain(self.product_id.0.to_le_bytes()) // 10-11
                .chain(self.device_version.0.to_le_bytes()) // 12-13
                .chain([
                    self.manufacturer_name.0,        // 14
                    self.product_name.0,             // 15
                    self.serial.0,                   // 16
                    self.configurations.len() as u8, // 17
                ]),
        )
    }
}

#[derive(Debug)]
pub struct ConfigurationDescriptor {
    /// wTotalLength (u16) is calculated based on serialization of,
    /// and bNumInterfaces (u8) is the length of:
    pub interfaces: Vec<InterfaceDescriptor>,

    /// bConfigurationValue.
    pub config_value: ConfigurationValue,

    /// iConfiguration.
    pub configuration_name: StringIndex,

    /// bmAttributes.
    pub attributes: ConfigurationAttributes,

    /// Descriptors of class-specific or vendor-specific augmentations.
    pub specific_augmentations: Vec<AugmentedDescriptor>,
}

impl Descriptor for ConfigurationDescriptor {
    /// bLength is 9 for Configuration Descriptor.
    /// (The combined length of other descriptors provided alongside it
    /// are given in wTotalLength)
    fn length(&self) -> u8 {
        9
    }

    /// bDescriptorType. 2 for Configuration Descriptor.
    fn descriptor_type(&self) -> DescriptorType {
        DescriptorType::Configuration
    }

    /// USB 2.0 table 9-8
    fn serialize(&self) -> Box<dyn Iterator<Item = u8> + '_> {
        // wTotalLength. Total length of all data returned when requesting
        // this descriptor, including interface and endpoint descriptors
        // and descriptors of class- and vendor- augmentations (e.g. HID).
        let total_length = self.length() as u16
            + self
                .interfaces
                .iter()
                .map(NestedDescriptor::total_length)
                .sum::<u16>();
        Box::new(
            self.header()
                .into_iter() // 0, 1
                .chain(total_length.to_le_bytes()) // 2-3
                .chain([
                    self.interfaces.len() as u8, // 4
                    self.config_value.0,         // 5
                    self.configuration_name.0,   // 6
                    self.attributes.0,           // 7
                    0, // 8. max power in 2mA units, hardcoding to 0
                ])
                .chain(
                    self.interfaces
                        .iter()
                        .flat_map(NestedDescriptor::serialize_all),
                ),
        )
    }
}

#[derive(Debug)]
pub struct InterfaceDescriptor {
    /// bInterfaceNumber
    pub interface_num: u8,

    /// bAlternateSetting.
    pub alternate_setting: u8,

    /// bNumEndpoints is the length of:
    pub endpoints: Vec<EndpointDescriptor>,

    /// bInterfaceClass.
    pub class: InterfaceClass, // u8

    /// bInterfaceSubClass.
    pub subclass: InterfaceSubclass, // u8

    /// bInterfaceProtocol.
    pub protocol: InterfaceProtocol, // u8,

    /// iInterface.
    pub interface_name: StringIndex, // u8

    /// Descriptors of class-specific or vendor-specific augmentations.
    pub specific_augmentations: Vec<AugmentedDescriptor>,
}

impl Descriptor for InterfaceDescriptor {
    /// bLength is 9 for Interface Descriptor.
    fn length(&self) -> u8 {
        9
    }

    /// bDescriptorType. 4 for Interface Descriptor.
    fn descriptor_type(&self) -> DescriptorType {
        DescriptorType::Interface
    }

    /// USB 2.0 table 9-12
    fn serialize(&self) -> Box<dyn Iterator<Item = u8> + '_> {
        Box::new(
            self.header()
                .into_iter() // 0, 1
                .chain([
                    self.interface_num,         // 2
                    self.alternate_setting,     // 3
                    self.endpoints.len() as u8, // 4
                    self.class.0,               // 5
                    self.subclass.0,            // 6
                    self.protocol.0,            // 7
                    self.interface_name.0,      // 8
                ]),
        )
    }
}

impl NestedDescriptor for InterfaceDescriptor {
    fn total_length(&self) -> u16 {
        self.length() as u16
            + self
                .specific_augmentations
                .iter()
                .map(|aug| aug.total_length())
                .sum::<u16>()
            + self
                .endpoints
                .iter()
                .map(|endpoint| endpoint.total_length())
                .sum::<u16>()
    }

    fn serialize_all(&self) -> Box<dyn Iterator<Item = u8> + '_> {
        Box::new(
            self.serialize()
                .chain(
                    self.specific_augmentations
                        .iter()
                        .flat_map(NestedDescriptor::serialize_all),
                )
                .chain(
                    self.endpoints
                        .iter()
                        .flat_map(NestedDescriptor::serialize_all),
                ),
        )
    }
}

#[derive(Debug)]
pub struct EndpointDescriptor {
    /// bEndpointAddress.
    pub endpoint_addr: u8,

    /// bmAttributes.
    pub attributes: EndpointAttributes,

    /// wMaxPacketSize. Largest packet endpoint is capable of transmitting.
    pub max_packet_size: u16,

    /// bInterval. Interval for polling transfers in frames on Interrupt and Isoch endpoints.
    /// Always 1 for Isoch. Ignored for Bulk and Control endpoints.
    pub interval: u8,

    /// Descriptors of class-specific or vendor-specific augmentations.
    pub specific_augmentations: Vec<AugmentedDescriptor>,
}

impl Descriptor for EndpointDescriptor {
    /// bLength. 7 for Endpoint Descriptor.
    fn length(&self) -> u8 {
        7
    }

    /// bDescriptorType. 5 for Endpoint Descriptor.
    fn descriptor_type(&self) -> DescriptorType {
        DescriptorType::Endpoint
    }

    /// USB 2.0 table 9-13
    fn serialize(&self) -> Box<dyn Iterator<Item = u8> + '_> {
        Box::new(
            self.header()
                .into_iter() // 0, 1
                .chain([self.endpoint_addr, self.attributes.0]) // 2, 3
                .chain(self.max_packet_size.to_le_bytes()) // 4-5
                .chain([self.interval]), // 6
        )
    }
}

impl NestedDescriptor for EndpointDescriptor {
    fn total_length(&self) -> u16 {
        self.length() as u16
            + self
                .specific_augmentations
                .iter()
                .map(|aug| aug.total_length())
                .sum::<u16>()
    }

    fn serialize_all(&self) -> Box<dyn Iterator<Item = u8> + '_> {
        Box::new(
            self.serialize().chain(
                self.specific_augmentations
                    .iter()
                    .flat_map(NestedDescriptor::serialize_all),
            ),
        )
    }
}

#[derive(Debug)]
pub enum AugmentedDescriptor {
    // HID(HidDescriptor)
}
impl Descriptor for AugmentedDescriptor {
    fn length(&self) -> u8 {
        todo!()
    }

    fn descriptor_type(&self) -> DescriptorType {
        todo!()
    }

    fn serialize(&self) -> Box<dyn Iterator<Item = u8> + '_> {
        todo!()
    }
}
impl NestedDescriptor for AugmentedDescriptor {
    fn total_length(&self) -> u16 {
        self.length() as u16
    }

    fn serialize_all(&self) -> Box<dyn Iterator<Item = u8> + '_> {
        self.serialize()
    }
}

#[derive(Debug)]
pub struct HidDescriptor {
    /// bcdHID. HID standard version.
    pub hid_version: Bcd16,

    /// bCountryCode.
    pub country_code: CountryCode,

    /// bNumDescriptors is the length of this Vec,
    /// which is followed by [bDescriptorType (u8), wDescriptorLength (u16)]
    /// for each descriptor at serialization time.
    pub class_descriptor: Vec<AugmentedDescriptor>,
}
impl Descriptor for HidDescriptor {
    /// bLength. Dependent on bNumDescriptors.
    fn length(&self) -> u8 {
        todo!()
    }

    /// bDescriptorType. 33 for HID Descriptor.
    fn descriptor_type(&self) -> DescriptorType {
        DescriptorType::HID
    }

    fn serialize(&self) -> Box<dyn Iterator<Item = u8> + '_> {
        todo!()
    }
}

#[derive(Debug)]
pub struct StringDescriptor {
    /// bString. Uses UTF-16 encoding in payloads.
    pub string: String,
}

impl Descriptor for StringDescriptor {
    /// UNUSED, but provided for completeness.
    /// To avoid doubling the calls to encode_utf16 in serialize,
    /// this is computed inline.
    // TODO: premature given that it costs an alloc and they're generally small?
    // but also they're requested infrequently enough to not matter either way.
    fn length(&self) -> u8 {
        (self.header().len()
            + (self.string.encode_utf16().count() * size_of::<u16>()))
            as u8
    }

    /// bDescriptorType. 3 for String Descriptor.
    fn descriptor_type(&self) -> DescriptorType {
        DescriptorType::String
    }

    fn serialize(&self) -> Box<dyn Iterator<Item = u8> + '_> {
        let utf16: Vec<u16> = self.string.encode_utf16().collect();
        let length = 2 + (utf16.len() * size_of::<u16>()) as u8;
        Box::new(
            [length, self.descriptor_type() as u8]
                .into_iter()
                .chain(utf16.into_iter().flat_map(|w| w.to_le_bytes())),
        )
    }
}

/// special-case for GET_DESCRIPTOR(String, 0)
#[derive(Debug)]
pub struct StringLanguageIdentifierDescriptor {
    /// wLANGID.
    pub language_ids: Vec<LanguageId>,
}

impl Descriptor for StringLanguageIdentifierDescriptor {
    /// bLength.
    fn length(&self) -> u8 {
        (self.header().len() + (self.language_ids.len() * size_of::<u16>()))
            as u8
    }

    /// bDescriptorType. 3, as it was with String Descriptor
    fn descriptor_type(&self) -> DescriptorType {
        DescriptorType::String
    }

    fn serialize(&self) -> Box<dyn Iterator<Item = u8> + '_> {
        Box::new(
            self.header().into_iter().chain(
                self.language_ids
                    .iter()
                    .flat_map(|langid| u16::from(langid).to_le_bytes()),
            ),
        )
    }
}

// USB 2.0 sect 11.23.1
#[derive(Debug)]
pub struct DeviceQualifierDescriptor {
    /// bcdUSB. USB version in binary-coded decimal.
    pub usb_version: Bcd16,
    /// bDeviceClass.
    pub device_class: ClassCode,
    /// bDeviceSubClass.
    pub device_subclass: SubclassCode,
    /// bDeviceProtocol.
    pub device_protocol: ProtocolCode,
    /// bMaxPacketSize0.
    pub max_packet_size_0: MaxSizeZeroEP,
    /// bNumConfigurations. Number of other-speed configurations
    pub num_configurations: u8,
}

impl Descriptor for DeviceQualifierDescriptor {
    fn length(&self) -> u8 {
        10
    }

    /// bDescriptorType. 6 for Device_Qualifier Descriptor.
    fn descriptor_type(&self) -> DescriptorType {
        DescriptorType::DeviceQualifier
    }

    fn serialize(&self) -> Box<dyn Iterator<Item = u8> + '_> {
        Box::new(
            self.header()
                .into_iter()
                .chain(self.usb_version.0.to_le_bytes())
                .chain([
                    self.device_class.0,
                    self.device_subclass.0,
                    self.device_protocol.0,
                    self.max_packet_size_0 as u8,
                    self.num_configurations,
                    0, // bReserved
                ]),
        )
    }
}
