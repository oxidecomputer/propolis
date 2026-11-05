// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::v1::components::backends;
use crate::v1::instance::{InstanceProperties, InstanceState};
use crate::v1::instance_spec::SpecKey;
use crate::v2::instance_spec::SmbiosType1Input;
use crate::v6;
use crate::v6::instance_spec::Component as V6Component;

pub use super::components::board::{Board, CpuTopology};

#[derive(Clone, Deserialize, Serialize, Debug, JsonSchema)]
pub struct InstanceSpec {
    pub board: Board,
    pub components: BTreeMap<SpecKey, V6Component>,
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

impl From<InstanceSpec> for v6::instance_spec::InstanceSpec {
    fn from(new: InstanceSpec) -> Self {
        Self {
            board: panic!("TODO: convert board down"), // new.board,
            components: new.components,
            smbios: new.smbios,
        }
    }
}

impl From<InstanceSpecStatus> for v6::instance_spec::InstanceSpecStatus {
    fn from(new: InstanceSpecStatus) -> Self {
        match new {
            InstanceSpecStatus::WaitingForMigrationSource => {
                Self::WaitingForMigrationSource
            }
            InstanceSpecStatus::Present(spec) => Self::Present(spec.into()),
        }
    }
}

impl From<InstanceSpecGetResponse>
    for v6::instance_spec::InstanceSpecGetResponse
{
    fn from(new: InstanceSpecGetResponse) -> Self {
        Self {
            properties: new.properties,
            state: new.state,
            spec: new.spec.into(),
        }
    }
}

impl From<v6::instance_spec::InstanceSpec> for InstanceSpec {
    fn from(old: v6::instance_spec::InstanceSpec) -> Self {
        let cpus = u16::from(old.board.cpus);
        Self {
            board: Board {
                cpus,
                memory_mb: old.board.memory_mb,
                chipset: old.board.chipset,
                guest_hv_interface: old.board.guest_hv_interface,
                cpuid: old.board.cpuid.map(|cpuid| cpuid.into()),
                cpu_topology: CpuTopology::legacy_single_thread_sockets(cpus),
            },
            components: old.components,
            smbios: old.smbios,
        }
    }
}
