// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

#![allow(dead_code)]

use bitstruct::bitstruct;

mod Registers {
    bitstruct! {
        /// Representation of the Status register.
        #[derive(Clone, Copy, Debug, Default, From)]
        pub struct Status(pub(crate) u8) {
            pub error: bool = 0;
            obsolete1: bool = 1;
            obsolete2: bool = 2;
            pub data_request: bool = 3;
            mode_specific_1: bool = 4;
            pub device_fault: bool = 5;
            pub device_ready: bool = 6;
            pub busy: bool = 7;
        }
    }

    bitstruct! {
        #[derive(Copy, Clone, PartialEq, Eq, From)]
        pub struct Device(pub(crate) u8) {
            pub address: u8 = 0..3;
            pub device_select: bool = 4;
            obsolete1: u8 = 5;
            pub lba_addressing: bool = 6;
            obsolete2: u8 = 7;
        }
    }

    impl Default for Device {
        fn default() -> Self {
            // Make sure default bits are set.
            Self(0xa0)
        }
    }
}
