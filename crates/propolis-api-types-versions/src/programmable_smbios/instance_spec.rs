// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Instance specification types for the PROGRAMMABLE_SMBIOS API version.

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::v1;
use crate::v1::components::board;
use crate::v1::instance::{InstanceProperties, InstanceState};
use crate::v1::instance_spec::{Component, SpecKey};

#[derive(Clone, Deserialize, Serialize, Debug, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SmbiosType1Input {
    pub manufacturer: String,
    pub product_name: String,
    pub serial_number: String,
    pub version: u64,
}

#[derive(Clone, Deserialize, Serialize, Debug, JsonSchema)]
pub struct InstanceSpec {
    pub board: board::Board,
    pub components: BTreeMap<SpecKey, Component>,
    pub smbios: Option<SmbiosType1Input>,
}

#[derive(Clone, Deserialize, Serialize, JsonSchema)]
#[serde(tag = "type", content = "value")]
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

impl From<InstanceSpec> for v1::instance_spec::InstanceSpec {
    fn from(new: InstanceSpec) -> Self {
        Self { board: new.board, components: new.components }
    }
}

impl From<InstanceSpecStatus> for v1::instance_spec::InstanceSpecStatus {
    fn from(new: InstanceSpecStatus) -> Self {
        match new {
            InstanceSpecStatus::WaitingForMigrationSource => {
                Self::WaitingForMigrationSource
            }
            InstanceSpecStatus::Present(spec) => Self::Present(
                v1::instance_spec::VersionedInstanceSpec::V0(spec.into()),
            ),
        }
    }
}

impl From<InstanceSpecGetResponse>
    for v1::instance_spec::InstanceSpecGetResponse
{
    fn from(new: InstanceSpecGetResponse) -> Self {
        Self {
            properties: new.properties,
            state: new.state,
            spec: new.spec.into(),
        }
    }
}

impl From<v1::instance_spec::InstanceSpec> for InstanceSpec {
    fn from(old: v1::instance_spec::InstanceSpec) -> Self {
        Self { board: old.board, components: old.components, smbios: None }
    }
}
