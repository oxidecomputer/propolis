// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! USB HID Class-specific request descriptor definitions

use report::ReportDescriptor;
use strum::FromRepr;

use super::descriptor::{Bcd16, CountryCode, Descriptor, DescriptorType};
use super::{requests::SetupData, Error};

pub mod report;

pub const HID_VER_1_11: Bcd16 = Bcd16(0x111);

/// USB HID 1.11 sect 7.2
#[repr(u8)]
#[derive(FromRepr, Debug)]
pub enum HIDRequest {
    GetReport = 1,
    GetIdle = 2,
    GetProtocol = 3,
    Reserved4 = 4,
    Reserved5 = 5,
    Reserved6 = 6,
    Reserved7 = 7,
    Reserved8 = 8,
    SetReport = 9,
    SetIdle = 10,
    SetProtocol = 11,
}

#[derive(Debug)]
pub enum HIDRequestInfo {
    /// Lets the host read a report through the control endpoint.
    /// (Typically, reports are read through a normal transfer on an
    /// Interrupt-IN endpoint, but a driver may choose to use this method.)
    /// USB HID 1.11 sect 7.2.1
    GetReport { report_type: HIDReportType, report_id: u8, interface: u16 },
    /// Sends a report to the device, possibly setting the state of
    /// input, output, or feature controls.
    /// USB HID 1.11 sect 7.2.2
    SetReport { report_type: HIDReportType, report_id: u8, interface: u16 },
    /// Reads the current idle rate for a particular Input report. (see SetIdle)
    /// USB HID 1.11 sect 7.2.3
    GetIdle { report_id: u8, interface: u16 },
    /// Silences a particular report on this endpoint until a new event occurs
    /// or the provided duration passes.
    /// USB HID 1.11 sect 7.2.4
    SetIdle {
        /// 0 = indefinite. Other values are in units of 4 milliseconds, e.g.
        /// 1u8 = 4ms, 255u8 = 1.020 seconds.
        duration_4ms: u8,
        report_id: u8,
        interface: u16,
    },
    /// Reads which of the boot or report protocol are active.
    /// USB HID 1.11 sect 7.2.5
    GetProtocol { interface: u16 },
    /// Switches between the boot and report protocols.
    /// USB HID 1.11 sect 7.2.6
    SetProtocol { protocol: HIDProtocol, interface: u16 },
}

impl TryFrom<SetupData> for HIDRequestInfo {
    type Error = Error;
    fn try_from(setup: SetupData) -> Result<Self, Self::Error> {
        match HIDRequest::from_repr(setup.request()) {
            Some(HIDRequest::GetReport) => {
                let [rtype, report_id] = setup.value().to_be_bytes();
                if let Some(report_type) = HIDReportType::from_repr(rtype) {
                    Ok(Self::GetReport {
                        report_type,
                        report_id,
                        interface: setup.index(),
                    })
                } else {
                    Err(Error::InvalidSetupParamsForRequest(
                        setup.request(),
                        setup.request_type(),
                        setup.value(),
                        setup.index(),
                    ))
                }
            }
            Some(HIDRequest::SetReport) => {
                let [rtype, report_id] = setup.value().to_be_bytes();
                if let Some(report_type) = HIDReportType::from_repr(rtype) {
                    Ok(Self::SetReport {
                        report_type,
                        report_id,
                        interface: setup.index(),
                    })
                } else {
                    Err(Error::InvalidSetupParamsForRequest(
                        setup.request(),
                        setup.request_type(),
                        setup.value(),
                        setup.index(),
                    ))
                }
            }
            Some(HIDRequest::GetIdle) => {
                let [0, report_id] = setup.value().to_be_bytes() else {
                    return Err(Error::InvalidSetupParamsForRequest(
                        setup.request(),
                        setup.request_type(),
                        setup.value(),
                        setup.index(),
                    ));
                };
                Ok(Self::GetIdle { report_id, interface: setup.index() })
            }
            Some(HIDRequest::SetIdle) => {
                let [duration_4ms, report_id] = setup.value().to_be_bytes();
                Ok(Self::SetIdle {
                    duration_4ms,
                    report_id,
                    interface: setup.index(),
                })
            }
            Some(HIDRequest::GetProtocol) => {
                Ok(Self::GetProtocol { interface: setup.index() })
            }
            Some(HIDRequest::SetProtocol) => {
                let Some(protocol) =
                    HIDProtocol::from_repr(setup.value() as u8)
                else {
                    return Err(Error::InvalidSetupParamsForRequest(
                        setup.request(),
                        setup.request_type(),
                        setup.value(),
                        setup.index(),
                    ));
                };
                Ok(Self::SetProtocol { protocol, interface: setup.index() })
            }
            Some(HIDRequest::Reserved4)
            | Some(HIDRequest::Reserved5)
            | Some(HIDRequest::Reserved6)
            | Some(HIDRequest::Reserved7)
            | Some(HIDRequest::Reserved8)
            | None => Err(Error::UnimplementedRequestType(
                setup.request(),
                setup.request_type(),
            )),
        }
    }
}

/// USB HID 1.11 sect 7.2.1
#[derive(Copy, Clone, FromRepr, Debug)]
#[repr(u8)]
pub enum HIDReportType {
    Input = 1,
    Output = 2,
    Feature = 3,
    // all other values reserved
}

/// USB HID 1.11 sect 7.2.5, 7.2.6
#[derive(FromRepr, Debug)]
#[repr(u8)]
pub enum HIDProtocol {
    Boot = 0,
    Report = 1,
}

#[derive(Debug)]
pub struct HIDDescriptor {
    /// bcdHID. HID standard version.
    pub hid_version: Bcd16,

    /// bCountryCode.
    pub country_code: CountryCode,

    /// bNumDescriptors is the length of this Vec,
    /// which is followed by [bDescriptorType (u8), wDescriptorLength (u16)]
    /// for each descriptor at serialization time.
    pub class_descriptor: Vec<ReportDescriptor>,
}
impl Descriptor for HIDDescriptor {
    /// bLength. Dependent on bNumDescriptors.
    fn length(&self) -> u8 {
        // bDescriptorType + wDescriptorLen for each
        6 + 3 * self.class_descriptor.len() as u8
    }

    /// bDescriptorType. 33 for HID Descriptor.
    fn descriptor_type(&self) -> DescriptorType {
        DescriptorType::HID
    }

    fn serialize(&self) -> Box<dyn Iterator<Item = u8> + '_> {
        Box::new(
            self.header()
                .into_iter() // 0, 1
                .chain(self.hid_version.0.to_le_bytes()) // 2, 3
                .chain([
                    self.country_code as u8,           // 4
                    self.class_descriptor.len() as u8, // 5
                ])
                .chain(self.class_descriptor.iter().flat_map(|x| {
                    [x.descriptor_type() as u8] // 6 + 3*n
                        .into_iter()
                        .chain((x.length() as u16).to_le_bytes()) // {7, 8} + 3*n
                })),
        )
    }
}
