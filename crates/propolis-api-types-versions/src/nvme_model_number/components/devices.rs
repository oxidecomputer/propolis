// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Device configuration data for the NVME_MODEL_NUMBER API version.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::v1;
use crate::v1::instance_spec::{PciPath, SpecKey};

/// A disk that presents an NVMe interface to the guest.
#[derive(Clone, Deserialize, Serialize, Debug, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NvmeDisk {
    /// The name of the disk's backend component.
    pub backend_id: SpecKey,

    /// The PCI bus/device/function at which this disk should be attached.
    pub pci_path: PciPath,

    /// The serial number to return in response to an NVMe Identify Controller
    /// command.
    #[serde(with = "serde_human_bytes::HexArray::<20>")]
    pub serial_number: [u8; 20],

    /// The model number to return in response to an NVMe Identify Controller
    /// command.
    #[serde(with = "serde_human_bytes::HexArray::<40>")]
    pub model_number: [u8; 40],
}

impl From<v1::components::devices::NvmeDisk> for NvmeDisk {
    fn from(old: v1::components::devices::NvmeDisk) -> Self {
        Self {
            backend_id: old.backend_id,
            pci_path: old.pci_path,
            serial_number: old.serial_number,
            model_number: [0u8; 40],
        }
    }
}

impl From<NvmeDisk> for v1::components::devices::NvmeDisk {
    fn from(new: NvmeDisk) -> Self {
        Self {
            backend_id: new.backend_id,
            pci_path: new.pci_path,
            serial_number: new.serial_number,
        }
    }
}
