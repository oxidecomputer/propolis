// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Disk and volume types for the CRUCIBLE_VOLUME_INFO API version.

use schemars::JsonSchema;
use serde::Serialize;

#[derive(Debug, Serialize, JsonSchema)]
pub struct VolumeStatus {
    pub volume_info: crucible_client_types::VolumeInfo,
}
