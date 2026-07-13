// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::v1::instance_spec::{PciPath, SpecKey};

/// Describes a PCI device implementing the eXtensible Host Controller Interface
/// for the purpose of attaching USB devices.
#[derive(Clone, Deserialize, Serialize, Debug, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct XhciController {
    /// The PCI path at which to attach the guest to this xHC.
    pub pci_path: PciPath,
}

/// Describes a USB device, requires the presence of an XhciController.
#[derive(Clone, Deserialize, Serialize, Debug, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct UsbDevice {
    /// The name of the xHC to which this USB device shall be attached.
    pub xhc_device: SpecKey,
    /// The root hub port number to which this USB device shall be attached.
    /// For USB 2.0 devices, valid values are 1-4, inclusive.
    /// For USB 3.0 devices, valid values are 5-8, inclusive.
    pub root_hub_port_num: u8,
    /// Which kind of supported USB device this is (e.g. `hid_tablet`)
    pub usb_device_type: UsbDeviceType,
}

#[derive(Clone, Deserialize, Serialize, Debug, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub enum UsbDeviceType {
    Null,
    /// Human Interface Device tablet for VNC pointer input support.
    HidTablet,
}
