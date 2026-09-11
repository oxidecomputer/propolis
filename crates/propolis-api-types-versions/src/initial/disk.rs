// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Disk and volume types for the INITIAL API version.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Clone, Deserialize, Serialize, JsonSchema)]
pub struct InstanceVCRReplace {
    pub vcr_json: String,
}

/// Path parameters for snapshot requests.
#[derive(Deserialize, JsonSchema)]
pub struct SnapshotRequestPathParams {
    pub id: String,
    pub snapshot_id: Uuid,
}

/// Path parameters for VCR requests.
#[derive(Deserialize, JsonSchema)]
pub struct VCRRequestPathParams {
    pub id: String,
}

/// Path parameters for volume status requests.
#[derive(Deserialize, JsonSchema)]
pub struct VolumeStatusPathParams {
    pub id: String,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct VolumeStatus {
    pub active: bool,
}
