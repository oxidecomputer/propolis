//! Artifact management.

use std::{
    collections::BTreeMap,
    fs::File,
    io::{BufReader, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{anyhow, bail, Result};
use ring::digest::{Digest, SHA256};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::{error, info, info_span, instrument};

use crate::guest_os::GuestOsKind;

#[derive(Debug, Error)]
pub enum ArtifactStoreError {
    #[error("The local root directory {0} does not exist")]
    LocalRootNotFound(PathBuf),

    #[error("The local root {0} is inaccessible or not a directory")]
    LocalRootNotDirectory(PathBuf),

    #[error(
        "One or more artifacts had invalid contents; check logs for details"
    )]
    ArtifactContentsInvalid(),
}

/// A single artifact.
#[derive(Debug, Serialize, Deserialize)]
struct ArtifactMetadata {
    /// The path to the artifact relative to the root directory specified in
    /// this artifact's store.
    relative_local_path: PathBuf,

    /// An optional SHA256 digest for this artifact. If present, the artifact
    /// store will use this digest to determine whether the artifact can be used
    /// or should be replaced, possibly with an artifact in remote storage.
    expected_digest: Option<String>,

    /// An optional path to this artifact relative to the file server root
    /// stored in the artifact store. If present, the store will use this to
    /// replace the artifact at startup if it appears to be corrupted.
    relative_remote_path: Option<String>,
}

impl ArtifactMetadata {
    fn check_local_artifact(
        &self,
        local_root: &Path,
        remote_root: Option<&str>,
    ) -> Result<()> {
        let mut local_path = PathBuf::new();
        local_path.push(local_root);
        local_path.push(&self.relative_local_path);

        // There are four possibilities:
        //
        // 1. The artifact doesn't exist at the expected path.
        // 2. The artifact exists, but has no digest recorded in the store.
        // 3. The artifact exists and has a digest recorded in the store,
        //    but the digest on disk doesn't match it.
        // 4. The artifact exists and has digest that matches what's in the
        //    store.
        //
        // In cases 1 and 3, try to redownload the artifact. In cases 2 and
        // 4, accept the artifact as-is and continue.
        let exists = local_path.exists();
        if exists {
            match &self.expected_digest {
                None => {
                    info!("Artifact exists but has no digest in its metadata");
                    return Ok(());
                }
                Some(digest) => match hash_equals(&local_path, digest) {
                    Ok(()) => {
                        info!("Artifact digest OK");
                        return Ok(());
                    }
                    Err(_) => {
                        info!("Artifact digest mismatched, will replace it");
                    }
                },
            }
        } else {
            info!("Artifact does not exist, will download it");
        }

        if exists {
            info!(?local_path, "Removing mismatched artifact before replacing");
            std::fs::remove_file(&local_path)?;
        }

        if remote_root.is_none() {
            return Err(anyhow!("Can't download artifact: no remote root"));
        }
        let remote_root = remote_root.unwrap();
        if self.relative_remote_path.is_none() {
            return Err(anyhow!("Can't download artifact: no remote path"));
        }
        let remote_relative = self.relative_remote_path.as_ref().unwrap();
        let remote_path = format!("{}/{}", remote_root, remote_relative);

        let download_timeout = Duration::from_secs(600);
        info!(
            ?local_path,
            ?remote_path,
            "Downloading artifact with timeout {:?}",
            download_timeout,
        );

        let client = reqwest::blocking::ClientBuilder::new()
            .timeout(download_timeout)
            .build()?;
        let request = client.get(remote_path).build()?;
        let response = client.execute(request)?;
        let mut new_file = std::fs::File::create(&local_path)?;
        new_file.write_all(&response.bytes()?)?;
        if let Some(digest) = &self.expected_digest {
            hash_equals(&local_path, digest)?;
        }

        Ok(())
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct GuestOsArtifact {
    guest_os_kind: GuestOsKind,
    metadata: ArtifactMetadata,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ArtifactStore {
    /// The root directory from which to construct paths to local artifacts.
    local_root: PathBuf,

    /// An optional remote file server from which to download artifacts whose
    /// digests do not match when the store is refreshed.
    remote_root: Option<String>,

    guest_images: BTreeMap<String, GuestOsArtifact>,
    bootroms: BTreeMap<String, ArtifactMetadata>,
}

impl ArtifactStore {
    pub fn from_file(path: impl AsRef<Path>) -> Result<Self> {
        info!(path = ?path.as_ref(), "Reading artifact store from file");
        let contents = std::fs::read_to_string(path.as_ref())?;
        Self::from_toml(&contents)
    }

    pub fn from_toml(raw_toml: &str) -> Result<Self> {
        let store: Self = toml::de::from_str(raw_toml)?;
        info!(?store, "Parsed artifact store");
        store.verify().map(|_| store)
    }

    pub fn get_local_root(&self) -> &Path {
        &self.local_root
    }

    pub fn get_guest_image_by_name(
        &self,
        artifact: &str,
    ) -> Option<(PathBuf, GuestOsKind)> {
        self.guest_images.get(artifact).map(|a| {
            (
                self.construct_full_path(&a.metadata.relative_local_path),
                a.guest_os_kind,
            )
        })
    }

    pub fn get_bootrom_by_name(&self, artifact: &str) -> Option<PathBuf> {
        self.bootroms
            .get(artifact)
            .map(|a| self.construct_full_path(&a.relative_local_path))
    }

    fn construct_full_path(&self, relative_path: &Path) -> PathBuf {
        let mut full = PathBuf::new();
        full.push(&self.local_root);
        full.push(relative_path);
        full
    }

    fn verify(&self) -> Result<()> {
        if !self.local_root.exists() {
            return Err(ArtifactStoreError::LocalRootNotFound(
                self.local_root.clone(),
            )
            .into());
        }
        if !self.local_root.is_dir() {
            return Err(ArtifactStoreError::LocalRootNotDirectory(
                self.local_root.clone(),
            )
            .into());
        }

        Ok(())
    }

    /// Verifies the existence and integrity of the local on-disk artifacts
    /// described by the store.
    ///
    /// Note: This routine may mutate artifacts on disk. This struct makes no
    /// attempt to synchronize these accesses between multiple threads. The
    /// caller is responsible for ensuring that it only checks local copies when
    /// no artifacts are otherwise in use.
    ///
    /// # Return value
    ///
    /// - `Ok` if all the artifacts exist and all the artifacts with digests in
    ///   store have matching digests on disk.
    /// - `Err(ArtifactStoreError::ArtifactContentsInvalid)` if one or more
    ///   artifacts could not be obtained or verified. The process logs contain
    ///   more information about the specific artifacts that failed and the
    ///   errors that caused those failures. Note that this routine checks all
    ///   artifacts in the store even if one fails.
    #[instrument(skip_all)]
    pub fn check_local_copies(&self) -> Result<()> {
        let mut all_ok = true;

        let iter = self
            .guest_images
            .iter()
            .map(|(k, v)| (k, &v.metadata))
            .chain(self.bootroms.iter());

        for (name, metadata) in iter {
            info!(?name, ?metadata, "Checking artifact");
            let _span = info_span!("Artifact {}", ?name);
            if let Err(e) = metadata.check_local_artifact(
                &self.local_root,
                self.remote_root.as_ref().map(|s| s.as_str()),
            ) {
                error!(?e, "Metadata check failed");
                all_ok = false;
            }
        }

        all_ok
            .then(|| ())
            .ok_or(ArtifactStoreError::ArtifactContentsInvalid().into())
    }
}

fn sha256_digest(file: &mut File) -> Result<Digest> {
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

fn hash_equals(path: impl AsRef<Path>, expected_digest: &str) -> Result<()> {
    let mut file = File::open(path.as_ref())?;
    let digest = hex::encode(sha256_digest(&mut file)?.as_ref());
    if digest != expected_digest {
        bail!(
            "Digest of {:#?} was {}, expected {}",
            path.as_ref(),
            digest,
            expected_digest
        );
    }
    Ok(())
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn store_to_from_toml() {
        let guest_artifact = GuestOsArtifact {
            guest_os_kind: GuestOsKind::Alpine,
            metadata: ArtifactMetadata {
                relative_local_path: "alpine.raw".into(),
                expected_digest: Some("abcd1234".to_string()),
                relative_remote_path: Some("alpine.raw".to_string()),
            },
        };

        let bootrom_artifact = ArtifactMetadata {
            relative_local_path: "OVMF_CODE.fd".into(),
            expected_digest: None,
            relative_remote_path: Some("OVMF_CODE.fd".to_string()),
        };

        let store = ArtifactStore {
            local_root: "/var/tmp/propolis-phd-images".into(),
            remote_root: Some("https://10.0.0.255".to_string()),
            guest_images: BTreeMap::from([(
                "alpine".to_string(),
                guest_artifact,
            )]),
            bootroms: BTreeMap::from([(
                "bootrom".to_string(),
                bootrom_artifact,
            )]),
        };

        let out = toml::ser::to_string(&store).unwrap();
        println!("TOML serialization output: {}", out);
        let _: ArtifactStore = toml::de::from_str(&out).unwrap();
    }

    #[test]
    fn verify_raw_toml() {
        let raw = r#"
            local_root = "/"
            remote_root = "https://10.0.0.255"

            [guest_images.alpine]
            guest_os_kind = "alpine"
            metadata.relative_local_path = "alpine.raw"
            metadata.expected_digest = "abcd1234"
            metadata.relative_remote_path = "alpine.raw"

            [bootroms.bootrom]
            relative_local_path = "OVMF_CODE.fd"
            relative_remote_path = "OVMF_CODE.fd"
        "#;

        let store = ArtifactStore::from_toml(raw).unwrap();
        println!("Generated store: {:#?}", store);
    }
}
