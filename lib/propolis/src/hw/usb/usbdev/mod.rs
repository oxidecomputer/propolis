// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use descriptor::DescriptorType;
use requests::{Request, RequestDirection};

pub mod descriptor;
pub mod requests;

pub mod demo_state_tracker;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("mismatched Setup and Data Stage transfer direction in transfer: {0:?} != {1:?}")]
    SetupVsDataDirectionMismatch(RequestDirection, RequestDirection),
    #[error("given an immediate for Out Data Stage")]
    ImmediateParameterForOutDataStage,
    #[error("In Data Stage memory write failed")]
    DataStageWriteFailed,
    #[error("Out Data Stage memory read failed")]
    DataStageReadFailed,
    #[error("expected Setup Stage before {0}")]
    NoSetupStageBefore(&'static str),
    #[error("matched Setup and Status Stage transfer direction {0:?}")]
    SetupVsStatusDirectionMatch(RequestDirection),
    #[error("unimplemented request {0:?}")]
    UnimplementedRequest(Request),
    #[error("invalid payload for {0:?} request: {1:#x?}")]
    InvalidPayloadForRequest(Request, Vec<u8>),
    #[error("unimplemented descriptor type: {0:?}")]
    UnimplementedDescriptor(DescriptorType),
    #[error("unknown descriptor type: {0:#x}")]
    UnknownDescriptorType(u8),
}

pub type Result<T> = core::result::Result<T, Error>;

#[usdt::provider(provider = "propolis")]
mod probes {
    fn usb_get_descriptor(descriptor_type: u8, index: u8) {}
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
