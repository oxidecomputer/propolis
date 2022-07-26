use anyhow::Result;
use phd_framework::artifacts::ArtifactStore;
use tracing::instrument;

use super::config;
use super::zfs::ZfsFixture;

/// A wrapper containing the objects needed to run the executor's test fixtures.
pub struct TestFixtures<'a> {
    artifact_store: &'a ArtifactStore,
    zfs: Option<ZfsFixture>,
}

impl<'a> TestFixtures<'a> {
    /// Creates a new set of test fixtures using the supplied command-line
    /// parameters and artifact store.
    pub fn new(
        runner_cfg: &config::Config,
        artifact_store: &'a ArtifactStore,
    ) -> Result<Self> {
        let zfs = runner_cfg
            .zfs_fs_name
            .as_ref()
            .map(|zfs_name| {
                let local_root = artifact_store
                    .get_local_root()
                    .to_string_lossy()
                    .to_string();
                ZfsFixture::new(zfs_name.clone(), &local_root)
            })
            .transpose()?;

        Ok(Self { artifact_store, zfs })
    }

    /// Calls fixture routines that need to run before any tests run.
    #[instrument(skip_all)]
    pub fn execution_setup(&mut self) -> Result<()> {
        // Set up the artifact store before setting up ZFS so that the ZFS
        // snapshot includes the up-to-date artifacts.
        self.artifact_store.check_local_copies()?;
        if let Some(zfs) = &mut self.zfs {
            zfs.create_artifact_snapshot()
        } else {
            Ok(())
        }
    }

    /// Calls fixture routines that need to run after all tests run.
    ///
    /// Unless the runner panics, or a test panics in a way that can't be caught
    /// during unwinding, this cleanup fixture will run even if a test run is
    /// interrupted.
    #[instrument(skip_all)]
    pub fn execution_cleanup(&mut self) -> Result<()> {
        if let Some(zfs) = &mut self.zfs {
            zfs.destroy_artifact_snapshot()
        } else {
            Ok(())
        }
    }

    /// Calls fixture routines that run before each test case is invoked.
    #[instrument(skip_all)]
    pub fn test_setup(&mut self) -> Result<()> {
        self.artifact_store.check_local_copies()
    }

    /// Calls fixture routines that run after each test case is invoked.
    ///
    /// Unless the runner panics, or a test panics in a way that can't be caught
    /// during unwinding, this cleanup fixture will run whenever the
    /// corresponding setup fixture has run.
    #[instrument(skip_all)]
    pub fn test_cleanup(&mut self) -> Result<()> {
        if let Some(zfs) = &mut self.zfs {
            zfs.rollback_to_artifact_snapshot()
        } else {
            Ok(())
        }
    }
}
