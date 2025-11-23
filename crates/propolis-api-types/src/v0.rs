// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Types used in the v0 API of `propolis-api-server`

use std::{collections::BTreeMap, net::SocketAddr};

use crate::instance_spec::{v0::InstanceSpecV0, SpecKey};
use crate::{InstanceProperties, ReplacementComponent};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(tag = "method", content = "value")]
pub enum InstanceInitializationMethodV0 {
    Spec {
        spec: InstanceSpecV0,
    },
    MigrationTarget {
        migration_id: Uuid,
        src_addr: SocketAddr,
        replace_components: BTreeMap<SpecKey, ReplacementComponent>,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
pub struct InstanceEnsureRequestV0 {
    pub properties: InstanceProperties,
    pub init: InstanceInitializationMethodV0,
}
