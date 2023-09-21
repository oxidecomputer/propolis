// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use anyhow::Result;
use serde::Deserialize;
use std::collections::BTreeMap;

#[derive(Clone, Debug, Deserialize)]
pub(super) struct Manifest {
    pub(super) remote_server_uris: Vec<String>,
    pub(super) artifacts: BTreeMap<String, super::Artifact>,
}

impl Manifest {
    pub(super) fn from_toml_path(toml_path: &camino::Utf8Path) -> Result<Self> {
        let contents = std::fs::read(toml_path.as_str())?;
        let toml_contents = String::from_utf8_lossy(&contents);
        Ok(toml::from_str(&toml_contents)?)
    }
}
