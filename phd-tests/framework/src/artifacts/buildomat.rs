use anyhow::Context;
use camino::Utf8Path;
use serde::{Deserialize, Serialize};
use std::{fmt, str::FromStr, time::Duration};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(transparent)]
pub(super) struct Repo(String);

#[derive(Clone, Debug, Serialize, Eq, PartialEq)]
#[serde(transparent)]
pub struct Commit(String);

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(transparent)]
pub(super) struct Series(String);

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BuildomatArtifact {
    pub(super) repo: Repo,
    pub(super) series: Series,
    pub(super) commit: Commit,
    pub(super) sha256: String,
}

const BASE_URI: &str = "https://buildomat.eng.oxide.computer/public";

impl Repo {
    pub(super) fn new(s: impl ToString) -> Self {
        Self(s.to_string())
    }

    pub(super) fn artifact_for_commit(
        self,
        series: Series,
        commit: Commit,
        filename: impl AsRef<Utf8Path>,
    ) -> anyhow::Result<BuildomatArtifact> {
        let filename = filename.as_ref();
        let sha256 = self.get_sha256(&series, &commit, filename)?;

        Ok(BuildomatArtifact { repo: self, series, commit, sha256 })
    }

    pub(super) fn artifact_for_branch_head(
        self,
        series: Series,
        branch: &str,
        filename: impl AsRef<Utf8Path>,
    ) -> anyhow::Result<BuildomatArtifact> {
        let commit =
            get_text_file(format!("{BASE_URI}/branch/{self}/{branch}"))
                .and_then(|s| Commit::from_str(&s))
                .with_context(|| {
                    format!(
                        "Failed to determine HEAD commit for {self}@{branch}"
                    )
                })?;
        self.artifact_for_commit(series, commit, filename)
    }

    fn get_sha256(
        &self,
        series: &Series,
        commit: &Commit,
        filename: &Utf8Path,
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
        get_text_file(format!(
            "{BASE_URI}/file/{self}/{series}/{commit}/{filename}.sha256.txt"
        ))
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

impl Series {
    pub(super) fn new(s: impl ToString) -> Self {
        Self(s.to_string())
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

fn get_text_file(url: impl AsRef<str>) -> anyhow::Result<String> {
    let url = url.as_ref();
    (|| {
        let client = reqwest::blocking::ClientBuilder::new()
            .timeout(Duration::from_secs(5))
            .build()?;
        let req = client.get(url).build()?;
        let rsp = client.execute(req)?;
        let status = rsp.status();
        anyhow::ensure!(
            status == reqwest::StatusCode::OK,
            "HTTP status: {status}"
        );

        let file = String::from_utf8(rsp.bytes()?.to_vec())?
            // the text file downloaded from Buildomat has a trailing newline,
            // so get rid of that...
            .trim()
            .to_string();
        Ok(file)
    })()
    .with_context(|| format!("Failed to download Buildomat text file {url:?}"))
}
