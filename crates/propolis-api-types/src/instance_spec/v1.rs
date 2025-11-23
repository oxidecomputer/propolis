// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::collections::BTreeMap;

use crate::instance_spec::{components, v0::ComponentV0, SpecKey};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Clone, Deserialize, Serialize, Debug, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct InstanceSpecV1 {
    pub board: components::board::Board,
    pub components: BTreeMap<SpecKey, ComponentV0>,
    pub smbios: SmbiosType1Input,
}

// Information put into the SMBIOS type 1 table in a VM
#[derive(Clone, Deserialize, Serialize, Debug, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SmbiosType1Input {
    pub manufacturer: String,
    pub product_name: String,
    pub serial_number: String,
    pub version: u64,
}
