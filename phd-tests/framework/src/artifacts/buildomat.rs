// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
use super::DownloadConfig;
use anyhow::Context;
use camino::Utf8Path;
use serde::{Deserialize, Serialize};
use std::{borrow::Cow, fmt, str::FromStr, time::Duration};
use tracing::{debug, warn};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(transparent)]
pub(super) struct Repo(Cow<'static, str>);

#[derive(Clone, Debug, Serialize, Eq, PartialEq)]
#[serde(transparent)]
pub struct Commit(String);

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(transparent)]
pub(super) struct Series(Cow<'static, str>);

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BuildomatArtifact {
    pub(super) repo: Repo,
    pub(super) series: Series,
    pub(super) commit: Commit,
    pub(super) sha256: String,
}

const BASE_URI: &str = "https://buildomat.eng.oxide.computer/public";

impl Repo {
    pub(super) const fn from_static(s: &'static str) -> Self {
        Self(Cow::Borrowed(s))
    }

    pub(super) fn artifact_for_commit(
        self,
        series: Series,
        commit: Commit,
        filename: impl AsRef<Utf8Path>,
        downloader: &DownloadConfig,
    ) -> anyhow::Result<BuildomatArtifact> {
        let filename = filename.as_ref();
        let sha256 = self.get_sha256(&series, &commit, filename, downloader)?;

        Ok(BuildomatArtifact { repo: self, series, commit, sha256 })
    }

    pub(super) fn get_branch_head(
        &self,
        branch: &str,
    ) -> anyhow::Result<Commit> {
        (|| {
            let uri = format!("{BASE_URI}/branch/{self}/{branch}");
            let client = reqwest::blocking::ClientBuilder::new()
                .timeout(Duration::from_secs(5))
                .build()?;
            let req = client.get(uri).build()?;
            let rsp = client.execute(req)?;
            let status = rsp.status();
            anyhow::ensure!(status.is_success(), "HTTP status: {status}");
            let bytes = rsp.bytes()?;
            str_from_bytes(&bytes)?.parse::<Commit>()
        })()
        .with_context(|| {
            format!("Failed to determine HEAD commit for {self}@{branch}")
        })
    }

    fn get_sha256(
        &self,
        series: &Series,
        commit: &Commit,
        filename: &Utf8Path,
        downloader: &DownloadConfig,
    ) -> anyhow::Result<String> {
        (|| {
            let filename = filename
                .file_name()
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "Buildomat filename has no filename: {filename:?}"
                    )
                })?
                // Strip the file extension, if any.
                //
                // Note: we use `Utf8PathBuf::file_name` and then split on '.'s
                // rather than using `Utf8PathBuf::file_stem`, because the latter
                // only strips off the rightmost file extension, rather than all
                // extensions. So, "foo.tar.gz" has a `file_stem()` of "foo.tar",
                // rather than "foo".
                //
                // TODO(eliza): `std::path::Path` has an unstable `file_prefix()`
                // method, which does exactly what we would want here (see
                // https://github.com/rust-lang/rust/issues/86319). If this is
                // stabilized, and `camino` adds a `file_prefix()` method wrapping
                // it, this code can be replaced with just `filename.file_prefix()`.
                .split('.')
                .next()
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "Buildomat filename has no filename prefix: {filename:?}"
                    )
                })?;
            let uri = format!("{BASE_URI}/file/{self}/{series}/{commit}/{filename}.sha256.txt");
            let bytes = downloader.download_buildomat_uri(&uri)?;
            str_from_bytes(&bytes).map(String::from)
        })().with_context(|| {
            format!("Failed to get SHA256 for {self}@{commit}, series: {series}, file: {filename})")
        })
    }
}

impl fmt::Display for Repo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
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

impl fmt::Display for Commit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
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

impl Series {
    pub(super) const fn from_static(s: &'static str) -> Self {
        Self(Cow::Borrowed(s))
    }
}

impl fmt::Display for Series {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl BuildomatArtifact {
    pub(super) fn uri(&self, filename: impl AsRef<Utf8Path>) -> String {
        let Self {
            repo: Repo(ref repo),
            series: Series(ref series),
            commit: Commit(ref commit),
            ..
        } = self;
        let filename = filename.as_ref();
        format!("{BASE_URI}/file/{repo}/{series}/{commit}/{filename}")
    }
}

impl fmt::Display for BuildomatArtifact {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Self {
            repo: Repo(ref repo),
            series: Series(ref series),
            commit: Commit(ref commit),
            ..
        } = self;
        write!(f, "Buildomat {repo}/{series}@{commit}")
    }
}

impl super::DownloadConfig {
    /// Download a file from the provided Buildomat URI.
    ///
    /// This method will retry the download if Buildomat returns an error that
    /// indicates a file does not yet exist, for up to the configurable maximum
    /// retry duration. This retry logic serves as a mechanism for PHD to wait
    /// for an artifact we expect to exist to be published, when the build that
    /// publishes that artifact is still in progress.
    pub(super) fn download_buildomat_uri(
        &self,
        uri: &str,
    ) -> anyhow::Result<bytes::Bytes> {
        debug!(
            timeout = ?self.timeout,
            %uri,
            "Downloading file from Buildomat...",
        );
        let client = reqwest::blocking::ClientBuilder::new()
            .timeout(self.timeout)
            .build()?;
        let try_download = || {
            let request = client
                .get(uri)
                .build()
                // failing to build the request is a permanent (non-retriable)
                // error, because any retries will use the same URI and request
                // configuration, so they'd fail as well.
                .map_err(|e| backoff::Error::permanent(e.into()))?;

            let response = client
                .execute(request)
                .map_err(|e| backoff::Error::transient(e.into()))?;
            if !response.status().is_success() {
                // when downloading a file from buildomat, we currently retry
                // all errors, since buildomat returns 500s when an artifact
                // doesn't exist. hopefully, this will be fixed upstream soon:
                // https://github.com/oxidecomputer/buildomat/pull/48
                let err = anyhow::anyhow!(
                    "Buildomat returned HTTP error {}",
                    response.status()
                );
                return Err(backoff::Error::transient(err));
            }
            Ok(response)
        };

        let log_retry = |error, wait| {
            warn!(
                %error,
                %uri,
                "Buildomat download failed, trying again in {wait:?}..."
            );
        };

        let bytes = backoff::retry_notify(
            self.buildomat_backoff.clone(),
            try_download,
            log_retry,
        )
        .map_err(|e| match e {
            backoff::Error::Permanent(e) => e,
            backoff::Error::Transient { err, .. } => err,
        })
        .with_context(|| format!("Failed to download '{uri}' from Buildomat"))?
        .bytes()?;

        Ok(bytes)
    }
}

fn str_from_bytes(bytes: &bytes::Bytes) -> anyhow::Result<&str> {
    Ok(std::str::from_utf8(bytes.as_ref())?.trim())
}
