// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Version `OPENAPI_LINT` of the Propolis Server API.
//!
//! This version makes the API conform to openapi-lint: enum values are
//! snake_case, `InstanceVCRReplace` is renamed to `InstanceVcrReplace`, and
//! endpoints that returned a JSON null now return 204 No Content.

pub mod api;
pub mod disk;
pub mod instance;
pub mod instance_spec;
pub mod migration;
