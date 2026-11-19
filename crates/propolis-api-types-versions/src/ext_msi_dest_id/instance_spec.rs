// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::components::board::{Board, InvalidV1GuestHypervisorInterface};
use crate::v1::instance::InstanceProperties;
use crate::v1::instance_spec::SpecKey;
use crate::v2::instance_spec::SmbiosType1Input;
use crate::v6;
use crate::v6::instance_spec::Component;
use crate::v7;
use crate::v7::instance::InstanceState;

#[derive(Clone, Deserialize, Serialize, Debug, JsonSchema)]
pub struct InstanceSpec {
    pub board: Board,
    pub components: BTreeMap<SpecKey, Component>,
    pub smbios: Option<SmbiosType1Input>,
}

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

impl From<v6::instance_spec::InstanceSpec> for InstanceSpec {
    fn from(old: v6::instance_spec::InstanceSpec) -> Self {
        let v6::instance_spec::InstanceSpec { board, components, smbios } = old;
        Self { board: board.into(), components, smbios }
    }
}

impl TryFrom<InstanceSpec> for v6::instance_spec::InstanceSpec {
    type Error = InvalidV1GuestHypervisorInterface;

    fn try_from(new: InstanceSpec) -> Result<Self, Self::Error> {
        let InstanceSpec { board, components, smbios } = new;
        Ok(Self { board: board.try_into()?, components, smbios })
    }
}

impl TryFrom<InstanceSpecStatus> for v7::instance_spec::InstanceSpecStatus {
    type Error = InvalidV1GuestHypervisorInterface;

    fn try_from(new: InstanceSpecStatus) -> Result<Self, Self::Error> {
        Ok(match new {
            InstanceSpecStatus::WaitingForMigrationSource => {
                Self::WaitingForMigrationSource
            }
            InstanceSpecStatus::Present(spec) => {
                Self::Present(spec.try_into()?)
            }
        })
    }
}

impl TryFrom<InstanceSpecGetResponse>
    for v7::instance_spec::InstanceSpecGetResponse
{
    type Error = InvalidV1GuestHypervisorInterface;

    fn try_from(new: InstanceSpecGetResponse) -> Result<Self, Self::Error> {
        let InstanceSpecGetResponse { properties, state, spec } = new;
        Ok(Self { properties, state, spec: spec.try_into()? })
    }
}
