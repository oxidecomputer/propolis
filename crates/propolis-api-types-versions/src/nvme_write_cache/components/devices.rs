// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use super::super::instance_spec;
use crate::v1::components::devices::NvmeDisk as V1NvmeDisk;
use crate::v1::instance_spec::{PciPath, SpecKey};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

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
    pub serial_number: [u8; 20],

    /// Control if the NVMe disk reports the presence of a volatile write cache.
    ///
    /// This generally should be configured in consideration of the storage
    /// backend for the NVMe device. "true" is a safe default, and was
    /// historically the only configurable value. If the storage backend will
    /// not lose data once writes are accepted, even in the face of unplanned
    /// crashes or power loss (or, if you really want to lie to guests), setting
    /// this to "false" can advise guests they may skip issuing flushes to the
    /// device.
    pub has_write_cache: bool,
}

impl TryFrom<NvmeDisk> for V1NvmeDisk {
    type Error = instance_spec::InvalidV3Component;

    fn try_from(disk: NvmeDisk) -> Result<Self, Self::Error> {
        let NvmeDisk { backend_id, pci_path, serial_number, has_write_cache } =
            disk;

        if !has_write_cache {
            return Err(instance_spec::InvalidV3Component {
                reason:
                    "NvmeDisk with has_write_cache=false cannot be downgraded",
            });
        }

        Ok(V1NvmeDisk { backend_id, pci_path, serial_number })
    }
}

impl From<V1NvmeDisk> for NvmeDisk {
    fn from(v1_disk: V1NvmeDisk) -> Self {
        let V1NvmeDisk { backend_id, pci_path, serial_number } = v1_disk;

        // API version `nvme_write_cache` pairs with a Propolis change to make
        // the former-default of NVMe devices offering `VWC=1` into a
        // configurable option. So the historical default and effect of any
        // previous APIs was to `has_write_cache: true`.
        Self { backend_id, pci_path, serial_number, has_write_cache: true }
    }
}
