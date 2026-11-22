// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::{collections::BTreeMap, net::SocketAddr};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::instance::ReplacementComponent;
use crate::v1::instance::InstanceProperties;
use crate::v1::instance_spec::SpecKey;
use crate::v6;
use crate::v6::instance_spec::InstanceSpec;

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(tag = "method", content = "value", rename_all = "snake_case")]
pub enum InstanceInitializationMethod {
    Spec {
        spec: InstanceSpec,
    },
    MigrationTarget {
        migration_id: Uuid,
        src_addr: SocketAddr,
        replace_components: BTreeMap<SpecKey, ReplacementComponent>,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
pub struct InstanceEnsureRequest {
    pub properties: InstanceProperties,
    pub init: InstanceInitializationMethod,
}

impl From<v6::api::InstanceInitializationMethod>
    for InstanceInitializationMethod
{
    fn from(old: v6::api::InstanceInitializationMethod) -> Self {
        match old {
            v6::api::InstanceInitializationMethod::Spec { spec } => {
                Self::Spec { spec }
            }
            v6::api::InstanceInitializationMethod::MigrationTarget {
                migration_id,
                src_addr,
                replace_components,
            } => Self::MigrationTarget {
                migration_id,
                src_addr,
                replace_components: replace_components
                    .into_iter()
                    .map(|(k, v)| (k, ReplacementComponent::from(v)))
                    .collect(),
            },
        }
    }
}

impl From<v6::api::InstanceEnsureRequest> for InstanceEnsureRequest {
    fn from(old: v6::api::InstanceEnsureRequest) -> Self {
        let v6::api::InstanceEnsureRequest { properties, init } = old;
        Self { properties, init: init.into() }
    }
}
