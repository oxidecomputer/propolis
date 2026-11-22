// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::instance::InstanceState;
use crate::v1::instance::InstanceProperties;
use crate::v6;
use crate::v6::instance_spec::InstanceSpec;

#[derive(Clone, Deserialize, Serialize, JsonSchema)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum InstanceSpecStatus {
    WaitingForMigrationSource,
    Present(InstanceSpec),
}

#[derive(Clone, Deserialize, Serialize, JsonSchema)]
pub struct InstanceSpecGetResponse {
    pub properties: InstanceProperties,
    pub state: InstanceState,
    pub spec: InstanceSpecStatus,
}

impl From<InstanceSpecStatus> for v6::instance_spec::InstanceSpecStatus {
    fn from(new: InstanceSpecStatus) -> Self {
        match new {
            InstanceSpecStatus::WaitingForMigrationSource => {
                Self::WaitingForMigrationSource
            }
            InstanceSpecStatus::Present(spec) => Self::Present(spec),
        }
    }
}

impl From<InstanceSpecGetResponse>
    for v6::instance_spec::InstanceSpecGetResponse
{
    fn from(new: InstanceSpecGetResponse) -> Self {
        let InstanceSpecGetResponse { properties, state, spec } = new;
        Self { properties, state: state.into(), spec: spec.into() }
    }
}
