// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Support for working with files consumed by PHD test runs.

use serde::{Deserialize, Serialize};

mod manifest;
mod store;

pub use store::Store as ArtifactStore;

pub const DEFAULT_PROPOLIS_ARTIFACT: &str = "__DEFAULT_PROPOLIS";

pub const CRUCIBLE_DOWNSTAIRS_ARTIFACT: &str = "crucible-downstairs";

#[derive(Clone, Debug, Serialize, Deserialize)]
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
    Buildomat { repo: String, series: String, commit: String, sha256: String },

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
}
