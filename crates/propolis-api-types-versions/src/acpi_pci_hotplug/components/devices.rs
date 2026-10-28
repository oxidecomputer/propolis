// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Clone, Deserialize, Serialize, Debug, JsonSchema, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Acpi {
    /// If true, PCI devices can be added to the instance without the need to
    /// stop it.
    pub enable_hotplug: bool,
}
