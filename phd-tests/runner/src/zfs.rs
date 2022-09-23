use std::process::{Command, Output};

use anyhow::{anyhow, Result};
use tracing::{error, info, warn};

/// A helper for implementing ZFS snapshots of on-disk artifacts as a fixture.
pub struct ZfsFixture {
    fs_name: String,
    snapshot_name: Option<String>,
}

impl ZfsFixture {
    /// Creates a new fixture.
    ///
    /// # Arguments
    ///
    /// - fs_name: The name of the ZFS filesystem object that should be the
    ///   subject of this fixture's ZFS commands.
    /// - expected_mountpoint: The directory at which the given file system
    ///   should be mounted, as reported by `zfs list`.
    pub fn new(fs_name: String, expected_mountpoint: &str) -> Result<Self> {
        let zfs_list_out =
            Command::new("zfs").args(["list", &fs_name]).output()?;
        let zfs_list_err =
            String::from_utf8_lossy(zfs_list_out.stderr.as_slice());
        let zfs_list_out =
            String::from_utf8_lossy(zfs_list_out.stdout.as_slice());

        info!(
            "zfs list {}\nstdout:\n{}\nstderr:\n{}",
            fs_name, zfs_list_out, zfs_list_err
        );
        if !zfs_list_out.starts_with("NAME") {
            return Err(anyhow!(
                "zfs output did not start with NAME:\n {}",
                zfs_list_out
            ));
        }

        let lines: Vec<&str> = zfs_list_out.lines().collect();
        if lines.len() != 2 {
            return Err(anyhow!(
                "zfs output had {} lines, expected 2",
                lines.len()
            ));
        }

        let mountpoint = lines[1].split_whitespace().last().unwrap();
        if mountpoint != expected_mountpoint {
            return Err(anyhow!(
                "zfs mountpoint {} not at expected mountpoint {}",
                mountpoint,
                expected_mountpoint
            ));
        }

        info!("ZFS initialization succeeded, will try using snapshots");
        Ok(Self { fs_name, snapshot_name: Default::default() })
    }

    /// Creates a base artifact snapshot. The fixture will roll back to this
    /// snapshot after every test.
    pub fn create_artifact_snapshot(&mut self) -> anyhow::Result<()> {
        let snapshot_name =
            format!("{}@phd_base_{}", self.fs_name, uuid::Uuid::new_v4());

        info!("Creating ZFS snapshot {}", snapshot_name);
        let zfs_out =
            Command::new("zfs").args(["snapshot", &snapshot_name]).output()?;

        if !zfs_out.stderr.is_empty() {
            return Err(anyhow!(
                "zfs snapshot failed, stderr: {}",
                String::from_utf8_lossy(zfs_out.stderr.as_slice())
            ));
        }

        self.snapshot_name = Some(snapshot_name);
        Ok(())
    }

    /// Destroys the base artifact snapshot.
    pub fn destroy_artifact_snapshot(&mut self) -> anyhow::Result<()> {
        let snapshot_name = self
            .snapshot_name
            .take()
            .expect("ZFS cleanup should occur after successful setup");
        info!("Deleting ZFS snapshot {}", snapshot_name);
        run_zfs_command(&["destroy", &snapshot_name])
    }

    /// Rolls back to this fixture's artifact snapshot.
    pub fn rollback_to_artifact_snapshot(&mut self) -> anyhow::Result<()> {
        let snapshot_name = self
            .snapshot_name
            .as_ref()
            .expect("ZFS test fixtures should occur after successful setup");
        info!("Rolling back to ZFS snapshot {}", snapshot_name);
        run_zfs_command(&["rollback", snapshot_name])
    }
}

fn run_zfs_command(args: &[&str]) -> Result<()> {
    warn_if_zfs_not_silent(args[0], Command::new("zfs").args(args).output()?);
    Ok(())
}

fn warn_if_zfs_not_silent(command: &str, zfs_output: Output) {
    if !zfs_output.stdout.is_empty() || !zfs_output.stderr.is_empty() {
        warn!(
            "ZFS {} command did not return silently\nstdout:\n{}\nstderr:\n{}",
            command,
            String::from_utf8_lossy(zfs_output.stdout.as_slice()),
            String::from_utf8_lossy(zfs_output.stderr.as_slice()),
        );
    }
}

// Try to destroy the base snapshot even if the fixture's owner fails to call
// the appropriate cleanup function.
impl Drop for ZfsFixture {
    fn drop(&mut self) {
        if let Some(snapshot_name) = self.snapshot_name.take() {
            if let Err(e) = run_zfs_command(&["destroy", &snapshot_name]) {
                error!(
                    ?e,
                    "Failed to run zfs destroy while tearing down ZFS fixture"
                );
            }
        }
    }
}
