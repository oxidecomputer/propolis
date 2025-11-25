// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Conversions from version-1 instance specs in the [`propolis_api_types`]
//! crate to the internal [`super::Spec`] representation.

use crate::spec::api_spec_v0::ApiSpecError;
use propolis_api_types::instance_spec::{
    v0::InstanceSpecV0, v1::InstanceSpecV1,
};
use thiserror::Error;

use super::Spec;
