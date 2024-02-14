// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Support functions for working with ZFS snapshots and clones.

use anyhow::Context;
use camino::{Utf8Path, Utf8PathBuf};
use tracing::{debug, error};
use uuid::Uuid;

#[derive(Debug)]
struct DatasetName(String);

#[derive(Debug)]
struct SnapshotName(String);

#[derive(Debug)]
struct CloneName(String);

/// Describes a dataset that's mounted at a specific point in the global
/// directory hierarchy.
#[derive(Debug)]
struct Dataset {
    /// The name of this dataset, used to refer to it as a subject of a ZFS
    /// operation.
    name: DatasetName,

    /// The mount point of this dataset. Stripping this prefix from the absolute
    /// path of a file that lies in this dataset yields the path to the file
    /// relative to the dataset root. This is needed to find the file if the
    /// dataset (or a clone of it) is mounted someplace else.
    mount_point: Utf8PathBuf,
}

/// Describes a snapshot of a specific dataset. When dropped, attempts to delete
/// itself using `zfs destroy`.
#[derive(Debug)]
struct Snapshot {
    /// The name of this snapshot, used to refer to it as a subject of a ZFS
    /// operation.
    name: SnapshotName,
}

impl Snapshot {
    /// Takes a snapshot of the supplied `dataset`.
    fn create_from_dataset(dataset: &DatasetName) -> anyhow::Result<Self> {
        let snapshot_name = format!("{}@phd-{}", dataset.0, Uuid::new_v4());
        zfs_command("snapshot", &[&snapshot_name])?;

        Ok(Self { name: SnapshotName(snapshot_name) })
    }
}

impl Drop for Snapshot {
    fn drop(&mut self) {
        debug!(name = self.name.0, "zfs snapshot dropped");
        let _ = zfs_command("destroy", &[&self.name.0]);
    }
}

/// Describes a clone of a specific snapshot. When dropped, attempts to delete
/// itself using `zfs destroy`.
#[derive(Debug)]
struct Clone {
    /// The name of this clone, used to refer to it as a subject of a ZFS
    /// operation.
    name: CloneName,

    /// The point at which this clone is mounted in the global directory
    /// hierarchy.
    mount_point: Utf8PathBuf,

    /// The snapshot this clone derives from. Snapshots can't be deleted until
    /// all their clones are gone; this reference helps to ensure that clones
    /// and snapshots are deleted in the correct order irrespective of when the
    /// clones are dropped.
    _snapshot: Snapshot,
}

impl Drop for Clone {
    fn drop(&mut self) {
        debug!(name = self.name.0, "zfs clone dropped");
        let _ = zfs_command("destroy", &[&self.name.0]);
    }
}

/// Represents a specific copy-on-write file within a ZFS clone. When this is
/// dropped, attempts to delete the associated clone.
#[derive(Debug)]
pub struct ClonedFile {
    /// The clone to which this file belongs.
    clone: Clone,

    /// The path to this file relative to the mount point of the clone.
    relative_path: Utf8PathBuf,
}

impl ClonedFile {
    /// Creates a snapshot and clone of the dataset that contains the canonical
    /// location of the file indicated by `path`.
    pub fn create_from_path(path: &Utf8Path) -> anyhow::Result<Self> {
        // Canonicalize the path to resolve any symbolic links before doing any
        // prefix matching.
        let canonical_path = path.canonicalize_utf8()?;

        let containing_dataset = Dataset::from_path(&canonical_path)
            .with_context(|| format!("getting dataset containing {path}"))?;

        let relative_file_path = canonical_path
            .strip_prefix(&containing_dataset.mount_point)
            .context("getting relative path to file to clone")?;

        let snapshot = Snapshot::create_from_dataset(&containing_dataset.name)?;
        Self::create_from_paths_and_snapshot(
            containing_dataset,
            relative_file_path,
            snapshot,
        )
        .with_context(|| {
            format!(
                "creating zfs clone of {canonical_path} with original path \
                {path}"
            )
        })
    }

    /// Yields the absolute path to this cloned file in the global directory
    /// hierarchy.
    pub fn path(&self) -> Utf8PathBuf {
        let mut path = self.clone.mount_point.clone();
        path.push(&self.relative_path);
        path
    }

    /// Given a path to a file relative to the root of its (mounted) dataset,
    /// and the name of a snapshot of that dataset, clones the snapshot and
    /// returns a handle to the clone. The [`path`] method can be used to find
    /// the absolute path to the file within the clone.
    fn create_from_paths_and_snapshot(
        dataset: Dataset,
        relative_file_path: &Utf8Path,
        snapshot: Snapshot,
    ) -> anyhow::Result<Self> {
        let clone_name =
            format!("{}/phd-clone-{}", dataset.name.0, Uuid::new_v4());

        zfs_command("clone", &[&snapshot.name.0, &clone_name])?;

        // If any errors occur between this point and the construction of a
        // `Clone` wrapper, this function needs to destroy the new clone
        // manually. The only thing needed to construct a `Clone` is its mount
        // point, so put that logic in a function and clean up manually if it
        // fails.
        fn get_clone_mount_point(
            clone_name: &str,
        ) -> anyhow::Result<Utf8PathBuf> {
            let output = zfs_command("list", &[&clone_name])?;
            let (object_name, mount_point) = parse_zfs_list_output(output)?;

            anyhow::ensure!(
                object_name == clone_name,
                "zfs list returned object {object_name} when asked about clone \
                {clone_name}"
            );

            let Some(mount_point) = mount_point else {
                anyhow::bail!("new zfs clone {clone_name} not mounted");
            };

            Ok(mount_point)
        }

        let mount_point = match get_clone_mount_point(&clone_name) {
            Ok(mount_point) => mount_point,
            Err(e) => {
                let _ = zfs_command("destroy", &[&clone_name]);
                return Err(e);
            }
        };

        Ok(Self {
            clone: Clone {
                name: CloneName(clone_name),
                mount_point,
                _snapshot: snapshot,
            },
            relative_path: relative_file_path.to_path_buf(),
        })
    }
}

impl Dataset {
    /// Looks up the dataset containing `path`.
    ///
    /// This routine fails if any `zfs` command line operations fail or return
    /// no output. It also fails if the found dataset's mount point is not a
    /// prefix of the supplied `path`, e.g. because of a symbolic link somewhere
    /// in the path.
    fn from_path(path: &Utf8Path) -> anyhow::Result<Self> {
        let output = zfs_command("list", &[path.as_str()])?;
        let (name, mount_point) = parse_zfs_list_output(output)
            .with_context(|| format!("parsing output from zfs list {path}"))?;

        let Some(mount_point) = mount_point else {
            anyhow::bail!(
                "`zfs list {path}` produced a dataset with no mount point"
            );
        };

        // The rest of this module needs to be able to strip the mount point
        // from the original path to get a dataset-relative path to the target
        // file. If the file path isn't prefixed by the mount point, this won't
        // work. This should generally not happen if the caller was diligent
        // about providing canonicalized paths.
        anyhow::ensure!(
            path.starts_with(&mount_point),
            "zfs dataset containing '{path}' is not prefixed by dataset mount
            point {mount_point} (is the path canonicalized?)"
        );

        Ok(Self { name: DatasetName(name.to_owned()), mount_point })
    }
}

/// Parses the output returned from a `zfs list` command into an object name and
/// a mountpoint.
///
/// This routine assumes the caller scoped its `zfs list` command so that it
/// returns exactly one (non-header) line of output. If it finds more, this
/// routine fails.
fn parse_zfs_list_output(
    output: std::process::Output,
) -> anyhow::Result<(String, Option<Utf8PathBuf>)> {
    let output = String::from_utf8(output.stdout)
        .context("converting `zfs list` output to string")?;

    debug!(stdout = output, "parsing zfs list output");

    // The expected output format from this command is
    //
    // NAME              USED  AVAIL     REFER  MOUNTPOINT
    // rpool/home/user   263G  549G       263G  /home/user
    let mut lines = output.lines();

    // Consume the header line and make sure it looks like it's sensibly
    // formatted. In particular, if the supplied path isn't part of a dataset,
    // `zfs` will return `cannot open 'path'`.
    let header = lines.next().ok_or_else(|| {
        anyhow::anyhow!("`zfs list` unexpectedly printed nothing")
    })?;

    anyhow::ensure!(
        header.starts_with("NAME"),
        "expected first line of `zfs list` output to start with NAME \
        (got '{header}')"
    );

    // Capture the first line of actual output for splitting and parsing. If
    // there are more output lines than this, fail instead of ignoring some
    // output.
    let answer = lines.next().ok_or_else(|| {
        anyhow::anyhow!("`zfs list` didn't have an output line")
    })?;

    if lines.next().is_some() {
        anyhow::bail!("`zfs list` returned more than one output line");
    }

    // `zfs list` output looks something like this (with a header line for
    // reference):
    //
    // NAME              USED  AVAIL     REFER  MOUNTPOINT
    // rpool/home/user   263G  549G       263G  /home/user
    //
    // The object name is the first token and the mount point is the fifth
    // (fourth after consuming the name).
    let mut words = answer.split_whitespace();
    let name = words.next().ok_or_else(|| {
        anyhow::anyhow!("`zfs list` didn't produce a dataset name")
    })?;

    // An unmounted object's mount point displays as "-", so this token should
    // always be present, even for unmounted objects.
    let mount_point = words.nth(3).ok_or_else(|| {
        anyhow::anyhow!("`zfs list` didn't produce a mount point")
    })?;

    let mount_point = mount_point
        .starts_with('/')
        .then_some(mount_point)
        .map(Utf8PathBuf::from);

    Ok((name.to_owned(), mount_point))
}

/// Executes `zfs <verb>` with the supplied `args` as trailing arguments.
/// Returns the full command output on success. Fails if `zfs` returned a
/// nonzero error code.
fn zfs_command(
    verb: &str,
    args: &[&str],
) -> anyhow::Result<std::process::Output> {
    debug!(verb, ?args, "executing ZFS command");

    let output = std::process::Command::new("pfexec")
        .arg("zfs")
        .arg(verb)
        .args(args)
        .output()
        .with_context(|| format!("running `zfs {verb}` with args {args:?}"))?;

    if !output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        error!(
            verb,
            ?args,
            error_code = output.status.code(),
            %stdout,
            %stderr,
            "zfs command failed"
        );
        anyhow::bail!(
            "`zfs {verb}` with args {args:?} returned error {:?}",
            output.status.code()
        );
    }

    Ok(output)
}
