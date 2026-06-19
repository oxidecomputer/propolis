// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use crate::v1::instance_spec::Component;
use crate::v1::instance_spec::SpecKey;
use crate::v3::instance_spec::InstanceSpec;

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
pub struct InstanceDiskAttachRequest {
    pub components: BTreeMap<SpecKey, Component>,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
pub struct InstanceDiskAttachResponse {
    pub spec: InstanceSpec,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
pub struct InstanceDiskDetachRequest {
    pub device_id: SpecKey,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
pub struct InstanceDiskDetachResponse {
    pub spec: InstanceSpec,
}
