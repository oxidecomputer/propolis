// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
use anyhow::{Context, Result};

pub(crate) fn workspace_root() -> Result<camino::Utf8PathBuf> {
    cargo_metadata::MetadataCommand::new()
        .no_deps()
        .exec()
        .context("Failed to run cargo metadata")
        .map(|meta| meta.workspace_root)
}
