// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::v1;

#[derive(Clone, Deserialize, Serialize, JsonSchema)]
pub struct InstanceVcrReplace {
    pub vcr_json: String,
}

impl From<v1::disk::InstanceVCRReplace> for InstanceVcrReplace {
    fn from(old: v1::disk::InstanceVCRReplace) -> Self {
        let v1::disk::InstanceVCRReplace { vcr_json } = old;
        Self { vcr_json }
    }
}
