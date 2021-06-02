use anyhow::{bail, Result};
use reqwest;
use ring::digest::{Context, Digest, SHA256};
use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use tokio::io::AsyncWriteExt;
use tokio::sync::OnceCell;

const S3_BUCKET: &str = "https://oxide-omicron-build.s3.amazonaws.com";

pub const TEST_IMAGE: Artifact<'static> = Artifact {
    name: "alpine.iso",
    expected_digest:
        "e1081276c21f5fceddc497f64624f2e23c88836bb829ff451aad7faad054b6c4",
};

pub const TEST_BOOTROM: Artifact<'static> = Artifact {
    name: "OVMF_CODE.fd",
    expected_digest:
        "32b4ba73f302e6c1c1ebd7ed0fecab9fa1fcedac77765454aa91e66604c10d27",
};

pub struct Artifact<'a> {
    name: &'a str,
    expected_digest: &'a str,
}

impl<'a> Artifact<'a> {
    fn source(&self) -> String {
        format!("{}/{}", S3_BUCKET, self.name)
    }

    /// Returns the path to the requested artifact.
    pub fn path(&self) -> PathBuf {
        let mut artifact_dir = artifact_directory();
        artifact_dir.push(&self.name);
        artifact_dir
    }

    async fn download(&self) -> Result<()> {
        let source = self.source();
        let destination = self.path();
        if destination.exists() {
            let r = hash_equals(&destination, &self.expected_digest);

            // If we fail to validate a previously existing hash, try removing
            // and re-downloading.
            match r {
                Ok(()) => return Ok(()),
                Err(_) => std::fs::remove_file(&destination)?,
            }
        }
        eprintln!(
            "Downloading {} to {}",
            source,
            destination.to_string_lossy()
        );
        let response = reqwest::get(source).await?;
        let mut file = tokio::fs::File::create(&destination).await?;
        file.write_all(&response.bytes().await?).await?;
        hash_equals(&destination, &self.expected_digest)?;
        Ok(())
    }
}

fn artifact_directory() -> PathBuf {
    let mut tmp = std::env::temp_dir();
    tmp.push("propolis-server-integration-test-artifacts");
    tmp
}

/// Calculates the SHA256 Digest of a file.
fn sha256_digest(file: &mut File) -> Result<Digest, std::io::Error> {
    file.seek(SeekFrom::Start(0))?;
    let mut reader = BufReader::new(file);
    let mut context = Context::new(&SHA256);
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

/// Returns Ok(()) iff the digest of the requested file matches the provided
/// digest.
fn hash_equals<P: AsRef<Path>>(path: P, expected_digest: &str) -> Result<()> {
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

// Used to ensure that the body of "setup" is only invoked once, no matter
// how many tests exist in this test suite.
static START: OnceCell<()> = OnceCell::const_new();

/// Initialize a directory full of artifacts which may be used for testing.
// TODO: Could actually return a reference to the Artifact objects after
// initialization, which might make it harder to use them uninitialized.
pub async fn setup() {
    START
        .get_or_init(|| async {
            let target = artifact_directory();
            let r = std::fs::create_dir(&target);
            if let Err(e) = r {
                assert_eq!(
                    e.kind(),
                    std::io::ErrorKind::AlreadyExists,
                    "Unexpected error creating tmp dir"
                );
            }
            TEST_IMAGE.download().await.unwrap();
            TEST_BOOTROM.download().await.unwrap();
        })
        .await;
}
