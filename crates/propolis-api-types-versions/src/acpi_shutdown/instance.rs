// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::v1::instance::InstanceStateRequested;

/// Requested state change of an Instance.
#[derive(Clone, Copy, Deserialize, Serialize, JsonSchema)]
pub struct InstanceStateChange {
    /// The desired state for the Instance.
    pub state: InstanceStateRequested,
    /// The number of seconds to wait after sending ACPI `PWRBTN_STS` before
    /// forcing stop/reset. (If omitted, stop/reset are forced immediately,
    /// and the ACPI signal is not sent.)
    pub acpi_timeout_secs: Option<u64>,
}
