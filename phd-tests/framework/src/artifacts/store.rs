// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use crate::{
    artifacts::{
        buildomat, manifest::Manifest, ArtifactKind, ArtifactSource,
        DownloadConfig, CRUCIBLE_DOWNSTAIRS_ARTIFACT,
        CURRENT_PROPOLIS_ARTIFACT, DEFAULT_PROPOLIS_ARTIFACT,
    },
    guest_os::GuestOsKind,
    CurrentPropolisSource,
};

use anyhow::{bail, Context};
use bytes::Bytes;
use camino::{Utf8Path, Utf8PathBuf};
use ring::digest::{Digest, SHA256};
use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufReader, Cursor, Read, Seek, SeekFrom, Write};
use std::sync::Mutex;
use std::time::Duration;
use tracing::{info, warn};

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
        downloader: &DownloadConfig,
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
                file_hash_equals(&path, digest)?;
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
            ArtifactSource::Buildomat(ref artifact) => &artifact.sha256,
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
            if file_hash_equals(&maybe_path, expected_digest).is_ok() {
                info!(%maybe_path,
                      "Valid artifact already exists, caching its path");
                return self.cache_path(maybe_path);
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
        // reacquire it.
        let bytes = match &self.description.source {
            ArtifactSource::Buildomat(source) => downloader
                .download_buildomat_artifact(
                    source,
                    &self.description.filename,
                    expected_digest,
                )?,
            ArtifactSource::RemoteServer { .. } => downloader
                .download_remote_artifact(
                    &self.description.filename,
                    expected_digest,
                )?,
            ArtifactSource::LocalPath { .. } => {
                unreachable!("local path case handled above")
            }
        };

        // There is at least one plausible place from which to try to obtain the
        // artifact. Create the directory that will hold it.
        std::fs::create_dir_all(maybe_path.parent().unwrap())?;
        let mut new_file = std::fs::File::create(&maybe_path)?;
        new_file.write_all(&bytes)?;

        // Make the newly-downloaded artifact read-only to try to
        // ensure tests won't change it. Disks created from an
        // artifact can be edited to be writable.
        let mut permissions = new_file.metadata()?.permissions();
        permissions.set_readonly(true);
        new_file.set_permissions(permissions)?;

        self.cache_path(maybe_path)
    }

    fn cache_path(
        &mut self,
        mut path: Utf8PathBuf,
    ) -> anyhow::Result<Utf8PathBuf> {
        if let Some(ref untar_path) = self.description.untar {
            // This artifact is a tarball, and a file must be extracted from it.
            let filename = untar_path.file_name().ok_or_else(|| {
                anyhow::anyhow!(
                    "untar path '{}' has no file name component",
                    untar_path
                )
            })?;
            let extracted_path = path.with_file_name(filename);

            path = if !extracted_path.exists() {
                info!(%extracted_path, %untar_path, "Extracting artifact from tarball");

                extract_tar_gz(&path, untar_path)?
            } else {
                info!(%extracted_path, "Artifact already extracted from tarball");
                extracted_path
            }
        };

        self.cached_path = Some(path.clone());
        Ok(path)
    }
}

#[derive(Debug)]
pub struct Store {
    local_dir: Utf8PathBuf,
    artifacts: BTreeMap<String, Mutex<StoredArtifact>>,
    downloader: DownloadConfig,
}

impl Store {
    pub fn from_toml_path(
        local_dir: Utf8PathBuf,
        toml_path: &Utf8Path,
        max_buildomat_wait: Duration,
    ) -> anyhow::Result<Self> {
        Ok(Self::from_manifest(
            local_dir,
            Manifest::from_toml_path(toml_path)?,
            max_buildomat_wait,
        ))
    }

    fn from_manifest(
        local_dir: Utf8PathBuf,
        manifest: Manifest,
        max_buildomat_wait: Duration,
    ) -> Self {
        let Manifest { artifacts, remote_server_uris } = manifest;
        let artifacts = artifacts
            .into_iter()
            .map(|(k, v)| (k, Mutex::new(StoredArtifact::new(v))))
            .collect();

        let buildomat_backoff = backoff::ExponentialBackoffBuilder::new()
            .with_max_elapsed_time(Some(max_buildomat_wait))
            .with_initial_interval(Duration::from_secs(1))
            .build();

        let store = Self {
            local_dir,
            artifacts,
            downloader: DownloadConfig {
                timeout: Duration::from_secs(600),
                buildomat_backoff,
                remote_server_uris,
            },
        };
        info!(?store, "Created new artifact store from manifest");
        store
    }

    pub fn add_propolis_from_local_cmd(
        &mut self,
        propolis_server_cmd: &Utf8Path,
    ) -> anyhow::Result<()> {
        tracing::info!(%propolis_server_cmd, "Adding Propolis server from local command");
        self.add_local_artifact(
            propolis_server_cmd,
            DEFAULT_PROPOLIS_ARTIFACT,
            ArtifactKind::PropolisServer,
        )
    }

    pub fn add_current_propolis(
        &mut self,
        source: crate::CurrentPropolisSource<'_>,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.artifacts.contains_key(CURRENT_PROPOLIS_ARTIFACT),
            "artifact store already contains key {CURRENT_PROPOLIS_ARTIFACT}",
        );

        const REPO: buildomat::Repo =
            buildomat::Repo::from_static("oxidecomputer/propolis");
        let commit = match source {
            CurrentPropolisSource::BuildomatBranch(branch) => {
                tracing::info!("Adding 'current' Propolis server from Buildomat Git branch '{branch}'");
                REPO.get_branch_head(branch)?
            }
            CurrentPropolisSource::BuildomatGitRev(commit) => {
                tracing::info!("Adding 'current' Propolis server from Buildomat Git commit '{commit}'");
                commit.clone()
            }
            CurrentPropolisSource::Local(cmd) => {
                tracing::info!("Adding 'current' Propolis server from local command '{cmd}'");
                return self.add_local_artifact(
                    cmd,
                    CURRENT_PROPOLIS_ARTIFACT,
                    ArtifactKind::PropolisServer,
                );
            }
        };

        // fetch the `phd_build` series, rather than the release `image` series,
        // to get a debug executable. the `phd_build` executable:
        // - contains debug assertions
        // - doesn't try to use the VMM kernel memory reservoir, which may not
        //   work nicely in the test environment
        let series = buildomat::Series::from_static("phd_build");
        let filename = Utf8PathBuf::from("propolis-server.tar.gz");
        let source = REPO.artifact_for_commit(
            series,
            commit,
            &filename,
            &self.downloader,
        )?;
        let artifact = super::Artifact {
            filename,
            kind: ArtifactKind::PropolisServer,
            source: ArtifactSource::Buildomat(source),
            untar: Some("propolis-server".into()),
        };

        let _old = self.artifacts.insert(
            CURRENT_PROPOLIS_ARTIFACT.to_string(),
            Mutex::new(StoredArtifact::new(artifact)),
        );
        assert!(_old.is_none());
        Ok(())
    }

    pub fn add_crucible_downstairs(
        &mut self,
        source: &crate::CrucibleDownstairsSource,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.artifacts.contains_key(CRUCIBLE_DOWNSTAIRS_ARTIFACT),
            "artifact store already contains key {CRUCIBLE_DOWNSTAIRS_ARTIFACT}",
        );

        match source {
            crate::CrucibleDownstairsSource::Local(
                ref crucible_downstairs_cmd,
            ) => {
                tracing::info!(%crucible_downstairs_cmd, "Adding crucible-downstairs from local command");
                self.add_local_artifact(
                    crucible_downstairs_cmd,
                    CRUCIBLE_DOWNSTAIRS_ARTIFACT,
                    ArtifactKind::CrucibleDownstairs,
                )
            }
            crate::CrucibleDownstairsSource::BuildomatGitRev(ref commit) => {
                tracing::info!(%commit, "Adding crucible-downstairs from Buildomat Git revision");
                let repo =
                    buildomat::Repo::from_static("oxidecomputer/crucible");
                let series = buildomat::Series::from_static("nightly-image");
                let filename = Utf8PathBuf::from("crucible-nightly.tar.gz");
                let artifact = repo.artifact_for_commit(
                    series,
                    commit.clone(),
                    &filename,
                    &self.downloader,
                )?;

                let artifact = super::Artifact {
                    filename,
                    kind: ArtifactKind::CrucibleDownstairs,
                    source: ArtifactSource::Buildomat(artifact),
                    untar: Some(
                        ["target", "release", "crucible-downstairs"]
                            .iter()
                            .collect::<Utf8PathBuf>(),
                    ),
                };

                let _old = self.artifacts.insert(
                    CRUCIBLE_DOWNSTAIRS_ARTIFACT.to_string(),
                    Mutex::new(StoredArtifact::new(artifact)),
                );
                assert!(_old.is_none());
                Ok(())
            }
        }
    }

    pub fn get_guest_os_image(
        &self,
        artifact_name: &str,
    ) -> anyhow::Result<(Utf8PathBuf, GuestOsKind)> {
        let entry = self.get_artifact(artifact_name)?;
        let mut guard = entry.lock().unwrap();
        match guard.description.kind {
            super::ArtifactKind::GuestOs(kind) => {
                let path = guard.ensure(&self.local_dir, &self.downloader)?;
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
                guard.ensure(&self.local_dir, &self.downloader)
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
                guard.ensure(&self.local_dir, &self.downloader)
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
                guard.ensure(&self.local_dir, &self.downloader)
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

    fn add_local_artifact(
        &mut self,
        cmd: &Utf8Path,
        artifact_name: &str,
        kind: super::ArtifactKind,
    ) -> anyhow::Result<()> {
        if self.artifacts.contains_key(artifact_name) {
            anyhow::bail!(
                "artifact store already contains key {artifact_name:?}"
            );
        }

        let full_path = cmd.canonicalize_utf8()?;
        let filename = full_path.file_name().ok_or_else(|| {
            anyhow::anyhow!(
                "local artifact command '{}' contains no file component",
                cmd
            )
        })?;
        let dir = full_path.parent().ok_or_else(|| {
            anyhow::anyhow!(
                "canonicalized local artifact path '{}' has no directory component",
                full_path
            )
        })?;

        let artifact = super::Artifact {
            filename: Utf8PathBuf::from(filename),
            kind,
            source: super::ArtifactSource::LocalPath {
                path: dir.to_path_buf(),
                sha256: None,
            },
            untar: None,
        };

        let _old: Option<Mutex<StoredArtifact>> = self.artifacts.insert(
            artifact_name.to_string(),
            Mutex::new(StoredArtifact::new(artifact)),
        );
        assert!(_old.is_none());

        Ok(())
    }
}

fn sha256_digest(mut reader: impl std::io::Read) -> anyhow::Result<Digest> {
    // file.seek(SeekFrom::Start(0))?;
    // let mut reader = BufReader::new(file);
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

fn file_hash_equals(
    path: impl AsRef<std::path::Path>,
    expected_digest: &str,
) -> anyhow::Result<()> {
    let file = File::open(path)?;
    let mut reader = BufReader::new(file);
    hash_equals(&mut reader, expected_digest)
}

fn hash_equals<R: Read + Seek>(
    mut bytes: R,
    expected_digest: &str,
) -> anyhow::Result<()> {
    // let mut file = File::open(path)?;
    bytes.seek(SeekFrom::Start(0))?;
    let digest = hex::encode(sha256_digest(&mut bytes)?.as_ref());
    if digest != expected_digest {
        bail!("Digest was {digest}, expected {expected_digest}");
    }

    Ok(())
}

fn extract_tar_gz(
    tarball_path: &Utf8Path,
    bin_path: &Utf8Path,
) -> anyhow::Result<Utf8PathBuf> {
    (|| {
        let dir_path =
            tarball_path.parent().context("Tarball path missing parent")?;

        if tarball_path.extension() == Some("gz") {
            tracing::debug!("Extracting gzipped tarball...");
            let file = File::open(tarball_path)?;
            let gz = flate2::read::GzDecoder::new(file);
            return extract_tarball(bin_path, dir_path, gz);
        }

        if tarball_path.extension() == Some("tar") {
            tracing::debug!("Extracting tarball...");
            let file = File::open(tarball_path)?;
            return extract_tarball(bin_path, dir_path, file);
        }

        bail!("File '{tarball_path}' is (probably) not a tarball?")
    })()
    .with_context(|| {
        format!(
            "Failed to extract file '{bin_path}' from tarball '{tarball_path}'"
        )
    })
}

fn extract_tarball(
    bin_path: &Utf8Path,
    dir_path: &Utf8Path,
    file: impl std::io::Read,
) -> anyhow::Result<Utf8PathBuf> {
    let mut archive = tar::Archive::new(file);

    let entries =
        archive.entries().context("Failed to iterate over tarball entries")?;
    for entry in entries {
        let mut entry = match entry {
            Ok(e) => e,
            Err(error) => {
                tracing::warn!(%error, "skipping bad tarball entry");
                continue;
            }
        };
        let path = entry.path().context("Tarball entry path was not UTF-8")?;
        if path == bin_path {
            let filename = bin_path
                .file_name()
                .expect("binary path in tarball must include a filename");
            let out_path = dir_path.join(filename);
            entry.unpack(&out_path).with_context(|| {
                format!(
                    "Failed to unpack '{bin_path}' from tarball to {out_path}"
                )
            })?;
            return Ok(out_path);
        }
    }

    Err(anyhow::anyhow!("No file named '{bin_path}' found in tarball"))
}

impl DownloadConfig {
    fn download_buildomat_artifact(
        &self,
        source: &buildomat::BuildomatArtifact,
        filename: &Utf8Path,
        expected_digest: &str,
    ) -> anyhow::Result<bytes::Bytes> {
        let bytes = self.download_buildomat_uri(&source.uri(filename))?;
        hash_equals(Cursor::new(bytes.as_ref()), expected_digest)?;
        Ok(bytes)
    }

    fn download_remote_artifact(
        &self,
        filename: &Utf8Path,
        expected_digest: &str,
    ) -> anyhow::Result<Bytes> {
        anyhow::ensure!(
            !self.remote_server_uris.is_empty(),
            "can't acquire artifact {filename} from remote server with no \
            remote URIs"
        );

        let client = reqwest::blocking::ClientBuilder::new()
            .timeout(self.timeout)
            .build()?;

        for remote in &self.remote_server_uris {
            let uri = format!("{remote}/{filename}");
            info!(timeout = ?self.timeout, "Downloading {filename} from {uri}");

            let request = client.get(&uri).build()?;
            let response = match client.execute(request) {
                Ok(resp) => resp,
                Err(e) => {
                    warn!(?e, uri, "Error obtaining artifact from source");
                    continue;
                }
            };

            if !response.status().is_success() {
                warn!(status = %response.status(), ?response, "HTTP error downloading {filename} from {uri}");
                continue;
            }

            let bytes = response.bytes()?;
            if let Err(error) =
                hash_equals(Cursor::new(bytes.as_ref()), expected_digest)
            {
                warn!(%error, "Hash mismatch downloading {filename} from {uri}");
                continue;
            } else {
                return Ok(bytes);
            }
        }

        Err(anyhow::anyhow!(
            "Failed to download {filename} from any remote URI",
        ))
    }
}
