// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::v1::instance::InstanceStateRequested as InstanceStateRequestedV1;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, JsonSchema)]
pub enum InstanceStateRequested {
    Run,
    Stop,
    Reboot,
    AcpiStop {
        /// The number of seconds to wait after sending ACPI `PWRBTN_STS` before
        /// forcing the guest to stop.
        timeout_secs: u64,
    },
    AcpiReboot {
        /// The number of seconds to wait after sending ACPI `PWRBTN_STS` before
        /// forcing the guest to reboot.
        timeout_secs: u64,
    },
}

impl From<InstanceStateRequestedV1> for InstanceStateRequested {
    fn from(value: InstanceStateRequestedV1) -> Self {
        match value {
            InstanceStateRequestedV1::Run => Self::Run,
            InstanceStateRequestedV1::Stop => Self::Stop,
            InstanceStateRequestedV1::Reboot => Self::Reboot,
        }
    }
}
