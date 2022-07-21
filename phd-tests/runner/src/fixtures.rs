use anyhow::Result;
use phd_framework::artifacts::ArtifactStore;
use tracing::instrument;

use super::config;
use super::zfs::ZfsFixture;

pub struct TestFixtures<'a> {
    artifact_store: &'a ArtifactStore,
    zfs: Option<ZfsFixture>,
}

impl<'a> TestFixtures<'a> {
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

    #[instrument(skip_all)]
    pub fn execution_setup(&mut self) -> Result<()> {
        // Set up the artifact store before setting up ZFS so that the ZFS
        // snapshot includes the up-to-date artifacts.
        self.artifact_store.check_local_copies()?;
        if let Some(zfs) = &mut self.zfs {
            zfs.execution_setup()
        } else {
            Ok(())
        }
    }

    #[instrument(skip_all)]
    pub fn execution_cleanup(&mut self) -> Result<()> {
        if let Some(zfs) = &mut self.zfs {
            zfs.execution_cleanup()
        } else {
            Ok(())
        }
    }

    #[instrument(skip_all)]
    pub fn test_setup(&mut self) -> Result<()> {
        self.artifact_store.check_local_copies()
    }

    #[instrument(skip_all)]
    pub fn test_cleanup(&mut self) -> Result<()> {
        if let Some(zfs) = &mut self.zfs {
            zfs.test_cleanup()
        } else {
            Ok(())
        }
    }
}
