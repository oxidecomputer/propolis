// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::v1::instance::InstanceStateRequested;

/// Requested state of an Instance.
#[derive(Clone, Copy, Deserialize, Serialize, JsonSchema)]
pub struct InstanceStateChange {
    pub state: InstanceStateRequested,
    // TODO doc
    pub acpi_timeout_secs: Option<u64>,
}
