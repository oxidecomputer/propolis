// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! API request and response types for the PROGRAMMABLE_SMBIOS API version.

use std::{collections::BTreeMap, net::SocketAddr};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::instance_spec::InstanceSpec;
use crate::v1;
use crate::v1::instance::{InstanceProperties, ReplacementComponent};
use crate::v1::instance_spec::SpecKey;

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(tag = "method", content = "value")]
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

impl From<v1::instance::InstanceInitializationMethod>
    for InstanceInitializationMethod
{
    fn from(old: v1::instance::InstanceInitializationMethod) -> Self {
        match old {
            v1::instance::InstanceInitializationMethod::Spec { spec } => {
                Self::Spec { spec: spec.into() }
            }
            v1::instance::InstanceInitializationMethod::MigrationTarget {
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

impl From<v1::instance::InstanceEnsureRequest> for InstanceEnsureRequest {
    fn from(old: v1::instance::InstanceEnsureRequest) -> Self {
        Self { properties: old.properties, init: old.init.into() }
    }
}
