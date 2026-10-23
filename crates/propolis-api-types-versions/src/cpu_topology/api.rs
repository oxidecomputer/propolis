// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! API request and response types for the TODO API version.

use std::{collections::BTreeMap, net::SocketAddr};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::instance_spec::InstanceSpec;
use crate::v1::instance::InstanceProperties;
use crate::v1::instance_spec::SpecKey;
use crate::v7;
use crate::v7::instance::ReplacementComponent;

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

impl From<v7::api::InstanceInitializationMethod>
    for InstanceInitializationMethod
{
    fn from(old: v7::api::InstanceInitializationMethod) -> Self {
        match old {
            v7::api::InstanceInitializationMethod::Spec { spec } => {
                Self::Spec { spec: spec.into() }
            }
            v7::api::InstanceInitializationMethod::MigrationTarget {
                migration_id,
                src_addr,
                replace_components,
            } => Self::MigrationTarget {
                migration_id,
                src_addr,
                replace_components,
            },
        }
    }
}

impl From<v7::api::InstanceEnsureRequest> for InstanceEnsureRequest {
    fn from(old: v7::api::InstanceEnsureRequest) -> Self {
        Self { properties: old.properties, init: old.init.into() }
    }
}
