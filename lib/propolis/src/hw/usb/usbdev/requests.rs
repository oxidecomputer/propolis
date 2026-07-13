// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use bitstruct::bitstruct;
use strum::FromRepr;

#[repr(u8)]
#[derive(FromRepr, Debug, PartialEq, Eq)]
pub enum RequestDirection {
    /// Also known as "OUT".
    HostToDevice = 0,
    /// Also known as "IN".
    DeviceToHost = 1,
}
impl From<bool> for RequestDirection {
    fn from(value: bool) -> Self {
        if value {
            Self::DeviceToHost
        } else {
            Self::HostToDevice
        }
    }
}
impl Into<bool> for RequestDirection {
    fn into(self) -> bool {
        self as u8 != 0
    }
}

#[repr(u8)]
#[derive(FromRepr, Debug)]
pub enum RequestType {
    Standard = 0,
    Class = 1,
    Vendor = 2,
    Reserved = 3,
}
impl From<u8> for RequestType {
    fn from(value: u8) -> Self {
        Self::from_repr(value).expect("RequestType should only be converted from a 2-bit field in Request")
    }
}
impl Into<u8> for RequestType {
    fn into(self) -> u8 {
        self as u8
    }
}

#[repr(u8)]
#[derive(FromRepr, Debug)]
pub enum RequestRecipient {
    Device = 0,
    Interface = 1,
    Endpoint = 2,
    Other = 3,
    Reserved4 = 4,
    Reserved5 = 5,
    Reserved6 = 6,
    Reserved7 = 7,
    Reserved8 = 8,
    Reserved9 = 9,
    Reserved10 = 10,
    Reserved11 = 11,
    Reserved12 = 12,
    Reserved13 = 13,
    Reserved14 = 14,
    Reserved15 = 15,
    Reserved16 = 16,
    Reserved17 = 17,
    Reserved18 = 18,
    Reserved19 = 19,
    Reserved20 = 20,
    Reserved21 = 21,
    Reserved22 = 22,
    Reserved23 = 23,
    Reserved24 = 24,
    Reserved25 = 25,
    Reserved26 = 26,
    Reserved27 = 27,
    Reserved28 = 28,
    Reserved29 = 29,
    Reserved30 = 30,
    Reserved31 = 31,
}
impl From<u8> for RequestRecipient {
    fn from(value: u8) -> Self {
        Self::from_repr(value).expect("RequestRecipient should only be converted from a 5-bit field in Request")
    }
}
impl Into<u8> for RequestRecipient {
    fn into(self) -> u8 {
        self as u8
    }
}

bitstruct! {
    /// USB 2.0 table 9-2.
    #[derive(Copy, Clone)]
    pub struct SetupData(pub u64) {
        /// Part of bmRequestType. Whether the request is addressed to the
        /// device, one of its interfaces, one of its endpoints, or
        /// otherwise.
        pub recipient: RequestRecipient = 0..5;
        /// Part of bmRequestType. Standard, Class, or Vendor.
        pub request_type: RequestType = 5..7;
        /// Part of bmRequestType. Data transfer direction.
        pub direction: RequestDirection = 7;
        /// bRequest. Specific type of request.
        /// Meaning varies according to bmRequestType.
        pub request: u8 = 8..16;
        /// wValue. Meaning varies according to bRequest.
        pub value: u16 = 16..32;
        /// wIndex. Meaning varies according to bRequest.
        /// Typically used to pass an index or offset.
        pub index: u16 = 32..48;
        /// wLength. Number of bytes to transfer if there is a Data Stage.
        pub length: u16 = 48..64;
    }
}
impl core::fmt::Debug for SetupData {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "SetupData {{ \
                recipient: {:?}, \
                request_type: {:?}, \
                direction: {:?}, \
                request: {}, \
                value: {}, \
                index: {}, \
                length: {}, \
            }}",
            self.recipient(),
            self.request_type(),
            self.direction(),
            self.request(),
            self.value(),
            self.index(),
            self.length()
        )
    }
}

/// USB 2.0 table 9-4
#[repr(u8)]
#[derive(FromRepr, Debug)]
pub enum StandardRequest {
    GetStatus = 0,
    ClearFeature = 1,
    Reserved2 = 2,
    SetFeature = 3,
    Reserved4 = 4,
    SetAddress = 5,
    GetDescriptor = 6,
    SetDescriptor = 7,
    GetConfiguration = 8,
    SetConfiguration = 9,
    GetInterface = 10,
    SetInterface = 11,
    SynchFrame = 12,
}
