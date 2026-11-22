// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::v7::instance::InstanceStateRequested as InstanceStateRequestedV7;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case", tag = "state", content = "value")]
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

impl From<InstanceStateRequestedV7> for InstanceStateRequested {
    fn from(value: InstanceStateRequestedV7) -> Self {
        match value {
            InstanceStateRequestedV7::Run => Self::Run,
            InstanceStateRequestedV7::Stop => Self::Stop,
            InstanceStateRequestedV7::Reboot => Self::Reboot,
        }
    }
}
