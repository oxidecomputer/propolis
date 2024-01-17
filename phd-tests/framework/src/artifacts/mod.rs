// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Support for working with files consumed by PHD test runs.

use serde::{Deserialize, Serialize};
use std::str::FromStr;

mod manifest;
mod store;

pub use store::Store as ArtifactStore;

pub const DEFAULT_PROPOLIS_ARTIFACT: &str = "__DEFAULT_PROPOLIS";

pub const CRUCIBLE_DOWNSTAIRS_ARTIFACT: &str = "__DEFAULT_CRUCIBLE_DOWNSTAIRS";

#[derive(Clone, Debug, Serialize, Eq, PartialEq)]
#[serde(transparent)]
pub struct Commit(String);

#[derive(Clone, Debug, Serialize, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
enum ArtifactKind {
    GuestOs(crate::guest_os::GuestOsKind),
    Bootrom,
    PropolisServer,
    CrucibleDownstairs,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ArtifactSource {
    /// Get the artifact from Buildomat. This downloads from
    /// https://buildomat.eng.oxide.computer/public/file/REPO/SERIES/COMMIT.
    Buildomat { repo: String, series: String, commit: Commit, sha256: String },

    /// Get the artifact from the manifest's list of remote artifact servers.
    RemoteServer { sha256: String },

    /// Get the artifact from the local file system.
    LocalPath { path: camino::Utf8PathBuf, sha256: Option<String> },
}

/// An individual artifact.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Artifact {
    /// The artifact file's name. When reacquiring an artifact from its source,
    /// this filename is appended to the URI generated from that source.
    filename: String,

    /// The kind of artifact this is.
    kind: ArtifactKind,

    /// The source to use to obtain this artifact if it's not present on the
    /// host system.
    source: ArtifactSource,

    /// If present, this artifact is a tarball, and the provided file should be
    /// extracted.
    untar: Option<camino::Utf8PathBuf>,
}

impl FromStr for Commit {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim();

        // Ensure this looks like a valid Git commit.
        anyhow::ensure!(
            s.len() == 40,
            "Buildomat requires full (40-character) Git commit hashes"
        );

        for c in s.chars() {
            if !c.is_ascii_hexdigit() {
                anyhow::bail!(
                    "'{c}' is not a valid hexadecimal digit; Git \
                    commit hashes should consist of the characters \
                    [0-9, a-f, A-F]"
                );
            }
        }

        Ok(Self(s.to_string()))
    }
}

impl std::fmt::Display for Commit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for Commit {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        FromStr::from_str(&s).map_err(serde::de::Error::custom)
    }
}
