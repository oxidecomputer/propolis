// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Support for working with files consumed by PHD test runs.

use serde::{Deserialize, Serialize};
use std::time::Duration;

pub mod buildomat;
mod manifest;
mod store;

pub use store::Store as ArtifactStore;

pub const DEFAULT_PROPOLIS_ARTIFACT: &str = "__DEFAULT_PROPOLIS";
pub const CRUCIBLE_DOWNSTAIRS_ARTIFACT: &str = "__DEFAULT_CRUCIBLE_DOWNSTAIRS";
pub const BASE_PROPOLIS_ARTIFACT: &str = "__BASE_PROPOLIS";

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
    Buildomat(buildomat::BuildomatArtifact),

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
    filename: camino::Utf8PathBuf,

    /// The kind of artifact this is.
    kind: ArtifactKind,

    /// The source to use to obtain this artifact if it's not present on the
    /// host system.
    source: ArtifactSource,

    /// If present, this artifact is a tarball, and the provided file should be
    /// extracted.
    untar: Option<camino::Utf8PathBuf>,
}

#[derive(Debug)]
struct DownloadConfig {
    timeout: Duration,
    /// Retry backoff settings used when downloading files from Buildomat.
    ///
    /// Retries for Buildomat artifact sources are configured separately from
    /// retries for remote URI artifact sources (which we don't currently retry;
    /// but probably should). This is because we use a very long maximum
    /// duration for retries for Buildomat artifacts, as a way of waiting for an
    /// in-progress build to complete (20 minutes by default). On the other
    /// hand, we probably don't want to retry a download from S3 for 20 minutes.
    buildomat_backoff: backoff::ExponentialBackoff,
    remote_server_uris: Vec<String>,
}
