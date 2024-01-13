// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use crate::{
    artifacts::{
        manifest::Manifest, ArtifactSource, CRUCIBLE_DOWNSTAIRS_ARTIFACT,
        DEFAULT_PROPOLIS_ARTIFACT,
    },
    guest_os::GuestOsKind,
};

use anyhow::{bail, Context};
use camino::{Utf8Path, Utf8PathBuf};
use ring::digest::{Digest, SHA256};
use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom, Write};
use std::sync::Mutex;
use std::time::Duration;
use tracing::info;

#[derive(Debug)]
struct StoredArtifact {
    description: super::Artifact,
    cached_path: Option<Utf8PathBuf>,
}

impl StoredArtifact {
    fn new(description: super::Artifact) -> Self {
        Self { description, cached_path: None }
    }

    fn ensure(
        &mut self,
        local_dir: &Utf8Path,
        remote_uris: &[String],
    ) -> anyhow::Result<Utf8PathBuf> {
        // If the artifact already exists and has been verified, return the path
        // to it straightaway.
        if let Some(path) = &self.cached_path {
            info!(%path, "Verified artifact already exists");
            return Ok(path.clone());
        }

        // If the manifest says to look for a local copy of the file, see if it
        // exists in the expected location and use it if it is.
        if let ArtifactSource::LocalPath { path, sha256 } =
            &self.description.source
        {
            let mut path = path.clone();
            path.push(self.description.filename.as_str());
            info!(%path, ?sha256, "Examining locally-sourced artifact");

            // Local files can have a digest but aren't required to have one.
            // This facilitates the use of local build outputs whose hashes
            // frequently change. If a digest was passed, make sure it matches.
            if let Some(digest) = sha256 {
                hash_equals(&path, digest)?;
            } else if !path.is_file() {
                anyhow::bail!("artifact path {} is not a file", path);
            }

            // The file is in the right place and has the right hash (if that
            // was checked), so mark it as cached and return the cached path.
            info!(%path, "Locally-sourced artifact is valid, caching its path");
            self.cached_path = Some(path.clone());
            return Ok(path.clone());
        }

        let expected_digest = match &self.description.source {
            ArtifactSource::Buildomat { sha256, .. } => sha256,
            ArtifactSource::RemoteServer { sha256 } => sha256,
            ArtifactSource::LocalPath { .. } => {
                unreachable!("local path case handled above")
            }
        };

        // See if the artifact already exists in the expected location in the
        // local artifact storage directory. If it does and it has the correct
        // digest, mark the artifact as present.
        let mut maybe_path = local_dir.to_path_buf();
        maybe_path
            .push(format!("{}/{}", expected_digest, self.description.filename));

        info!(%maybe_path, "checking for existing copy of artifact");
        if maybe_path.is_file() {
            if hash_equals(&maybe_path, expected_digest).is_ok() {
                info!(%maybe_path,
                      "Valid artifact already exists, caching its path");

                self.cached_path = Some(maybe_path.clone());
                return Ok(maybe_path);
            } else {
                info!(%maybe_path, "Existing artifact is invalid, deleting it");
                std::fs::remove_file(&maybe_path)?;
            }
        } else if maybe_path.exists() {
            anyhow::bail!(
                "artifact path {} already exists but isn't a file",
                maybe_path
            );
        }

        // The artifact is not in the expected place or has the wrong digest, so
        // reacquire it. First, construct the set of source URIs from which the
        // artifact can be reacquired.
        let source_uris = match &self.description.source {
            ArtifactSource::Buildomat { repo, series, commit, .. } => {
                let buildomat_uri = buildomat_url(
                    repo,
                    series,
                    commit,
                    &self.description.filename,
                );

                vec![buildomat_uri]
            }
            ArtifactSource::RemoteServer { .. } => {
                if remote_uris.is_empty() {
                    anyhow::bail!(
                        "can't acquire artifact from remote server with no \
                         remote URIs"
                    );
                }

                remote_uris
                    .iter()
                    .map(|uri| format!("{}/{}", uri, self.description.filename))
                    .collect()
            }
            ArtifactSource::LocalPath { .. } => {
                unreachable!("local path case handled above")
            }
        };

        // There is at least one plausible place from which to try to obtain the
        // artifact. Create the directory that will hold it.
        std::fs::create_dir_all(maybe_path.parent().unwrap())?;
        let download_timeout = Duration::from_secs(600);
        for uri in &source_uris {
            info!(%maybe_path,
                  uri,
                  "Downloading artifact with timeout {:?}",
                  download_timeout);

            let client = reqwest::blocking::ClientBuilder::new()
                .timeout(download_timeout)
                .build()?;

            let request = client.get(uri).build()?;
            let response = match client.execute(request) {
                Ok(resp) => resp,
                Err(e) => {
                    info!(?e, uri, "Error obtaining artifact from source");
                    continue;
                }
            };

            let mut new_file = std::fs::File::create(&maybe_path)?;
            new_file.write_all(&response.bytes()?)?;
            match hash_equals(&maybe_path, expected_digest) {
                Ok(_) => {
                    // Make the newly-downloaded artifact read-only to try to
                    // ensure tests won't change it. Disks created from an
                    // artifact can be edited to be writable.
                    let mut permissions = new_file.metadata()?.permissions();
                    permissions.set_readonly(true);
                    new_file.set_permissions(permissions)?;
                    self.cached_path = Some(maybe_path.clone());
                    return Ok(maybe_path);
                }
                Err(e) => {
                    info!(?e, uri, "Failed to verify digest for artifact");
                }
            }
        }

        Err(anyhow::anyhow!(
            "failed to locate or obtain artifact at path {}",
            maybe_path
        ))
    }
}

#[derive(Debug)]
pub struct Store {
    local_dir: Utf8PathBuf,
    artifacts: BTreeMap<String, Mutex<StoredArtifact>>,
    remote_server_uris: Vec<String>,
}

impl Store {
    pub fn from_toml_path(
        local_dir: Utf8PathBuf,
        toml_path: &Utf8Path,
    ) -> anyhow::Result<Self> {
        Ok(Self::from_manifest(local_dir, Manifest::from_toml_path(toml_path)?))
    }

    fn from_manifest(local_dir: Utf8PathBuf, manifest: Manifest) -> Self {
        let Manifest { artifacts, remote_server_uris } = manifest;
        let artifacts = artifacts
            .into_iter()
            .map(|(k, v)| (k, Mutex::new(StoredArtifact::new(v))))
            .collect();

        let store = Self { local_dir, artifacts, remote_server_uris };
        info!(?store, "Created new artifact store from manifest");
        store
    }

    pub fn add_propolis_from_local_cmd(
        &mut self,
        propolis_server_cmd: &Utf8Path,
    ) -> anyhow::Result<()> {
        if self.artifacts.contains_key(DEFAULT_PROPOLIS_ARTIFACT) {
            anyhow::bail!(
                "artifact store already contains key {}",
                DEFAULT_PROPOLIS_ARTIFACT
            );
        }

        let full_path = propolis_server_cmd.canonicalize_utf8()?;
        let filename = full_path.file_name().ok_or_else(|| {
            anyhow::anyhow!(
                "Propolis server command '{}' contains no file component",
                propolis_server_cmd
            )
        })?;
        let dir = full_path.parent().ok_or_else(|| {
            anyhow::anyhow!(
                "canonicalized Propolis path '{}' has no directory component",
                full_path
            )
        })?;

        let artifact = super::Artifact {
            filename: filename.to_string(),
            kind: super::ArtifactKind::PropolisServer,
            source: super::ArtifactSource::LocalPath {
                path: dir.to_path_buf(),
                sha256: None,
            },
        };

        let _old = self.artifacts.insert(
            DEFAULT_PROPOLIS_ARTIFACT.to_string(),
            Mutex::new(StoredArtifact::new(artifact)),
        );
        assert!(_old.is_none());
        Ok(())
    }

    pub fn add_crucible_downstairs_from_rev(
        &mut self,
        rev: &str,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(!self.artifacts.contains_key(CRUCIBLE_DOWNSTAIRS_ARTIFACT), "artifact store already contains key {CRUCIBLE_DOWNSTAIRS_ARTIFACT}");

        const REPO: &str = "oxidecomputer/crucible";
        const SERIES: &str = "nightly-image";

        let sha256 = (|| {
            let client = reqwest::blocking::ClientBuilder::new()
                .timeout(Duration::from_secs(5))
                .build()?;
            let req = client
                .get(buildomat_url(
                    REPO,
                    SERIES,
                    rev,
                    "crucible-nightly.sha256.txt",
                ))
                .build()?;
            let bytes = client.execute(req)?.bytes()?;
            Ok::<_, anyhow::Error>(String::from_utf8(bytes.to_vec())?)
        })()
        .with_context(|| {
            format!("Failed to get Buildomat SHA256 for {REPO}/{SERIES}/{rev}")
        })?;

        let artifact = super::Artifact {
            filename: "crucible-nightly.tar.gz".to_string(),
            kind: super::ArtifactKind::CrucibleDownstairs,
            source: super::ArtifactSource::Buildomat {
                repo: "oxidecomputer/crucible".to_string(),
                series: "nightly-image".to_string(),
                commit: rev.to_string(),
                sha256,
            },
        };

        let _old = self.artifacts.insert(
            CRUCIBLE_DOWNSTAIRS_ARTIFACT.to_string(),
            Mutex::new(StoredArtifact::new(artifact)),
        );
        assert!(_old.is_none());
        Ok(())
    }

    pub fn get_guest_os_image(
        &self,
        artifact_name: &str,
    ) -> anyhow::Result<(Utf8PathBuf, GuestOsKind)> {
        let entry = self.get_artifact(artifact_name)?;
        let mut guard = entry.lock().unwrap();
        match guard.description.kind {
            super::ArtifactKind::GuestOs(kind) => {
                let path =
                    guard.ensure(&self.local_dir, &self.remote_server_uris)?;
                Ok((path, kind))
            }
            _ => Err(anyhow::anyhow!(
                "artifact {} is not a guest OS image",
                artifact_name
            )),
        }
    }

    pub fn get_bootrom(
        &self,
        artifact_name: &str,
    ) -> anyhow::Result<Utf8PathBuf> {
        let entry = self.get_artifact(artifact_name)?;
        let mut guard = entry.lock().unwrap();
        match guard.description.kind {
            super::ArtifactKind::Bootrom => {
                guard.ensure(&self.local_dir, &self.remote_server_uris)
            }
            _ => Err(anyhow::anyhow!(
                "artifact {} is not a bootrom",
                artifact_name
            )),
        }
    }

    pub fn get_propolis_server(
        &self,
        artifact_name: &str,
    ) -> anyhow::Result<Utf8PathBuf> {
        let entry = self.get_artifact(artifact_name)?;
        let mut guard = entry.lock().unwrap();
        match guard.description.kind {
            super::ArtifactKind::PropolisServer => {
                guard.ensure(&self.local_dir, &self.remote_server_uris)
            }
            _ => Err(anyhow::anyhow!(
                "artifact {} is not a Propolis server",
                artifact_name
            )),
        }
    }

    pub fn get_crucible_downstairs(&self) -> anyhow::Result<Utf8PathBuf> {
        let entry = self.get_artifact(CRUCIBLE_DOWNSTAIRS_ARTIFACT)?;
        let mut guard = entry.lock().unwrap();
        match guard.description.kind {
            super::ArtifactKind::CrucibleDownstairs => {
                guard.ensure(&self.local_dir, &self.remote_server_uris)
            }
            _ => Err(anyhow::anyhow!(
                "artifact {CRUCIBLE_DOWNSTAIRS_ARTIFACT} is not a Crucible downstairs binary",
            )),
        }
    }

    fn get_artifact(
        &self,
        name: &str,
    ) -> anyhow::Result<&Mutex<StoredArtifact>> {
        self.artifacts.get(name).ok_or_else(|| {
            anyhow::anyhow!("artifact {} not found in store", name)
        })
    }
}

fn buildomat_url(
    repo: impl AsRef<str>,
    series: impl AsRef<str>,
    commit: impl AsRef<str>,
    file: impl AsRef<str>,
) -> String {
    format!(
        "https://buildomat.eng.oxide.computer/public/file/public/file/{}/{}/{}/{}",
        repo.as_ref(), series.as_ref(), commit.as_ref(), file.as_ref(),
    )
}

fn sha256_digest(file: &mut File) -> anyhow::Result<Digest> {
    file.seek(SeekFrom::Start(0))?;
    let mut reader = BufReader::new(file);
    let mut context = ring::digest::Context::new(&SHA256);
    let mut buffer = [0; 1024];

    loop {
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        context.update(&buffer[..count]);
    }

    Ok(context.finish())
}

fn hash_equals(path: &Utf8Path, expected_digest: &str) -> anyhow::Result<()> {
    let mut file = File::open(path)?;
    let digest = hex::encode(sha256_digest(&mut file)?.as_ref());
    if digest != expected_digest {
        bail!(
            "Digest of {} was {}, expected {}",
            path,
            digest,
            expected_digest
        );
    }

    Ok(())
}
