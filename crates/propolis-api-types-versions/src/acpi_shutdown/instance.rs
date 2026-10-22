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
    AcpiStop,
    AcpiReboot,
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

/// Requested state change of an Instance.
#[derive(Clone, Copy, Deserialize, Serialize, JsonSchema)]
pub struct InstanceStateChange {
    /// The desired state for the Instance.
    pub state: InstanceStateRequested,
    /// The number of seconds to wait after sending ACPI `PWRBTN_STS` before
    /// forcing stop/reset. (Only valid for AcpiStop and AcpiReboot states.)
    pub acpi_timeout_secs: Option<u64>,
}
