// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use anyhow::Context;
use camino::Utf8PathBuf;
use clap::{Args, Parser, Subcommand};
use phd_framework::{
    artifacts, server_log_mode::ServerLogMode, CrucibleDownstairsSource,
    CurrentPropolisSource,
};
use std::str::FromStr;

#[derive(Debug, Subcommand)]
#[allow(clippy::large_enum_variant)]
pub enum Command {
    Run(RunOptions),
    List(ListOptions),
}

/// Runtime configuration options for the runner.
#[derive(Debug, Parser)]
#[clap(verbatim_doc_comment)]
pub struct ProcessArgs {
    #[clap(subcommand)]
    pub command: Command,

    /// Suppress emission of terminal control codes in the runner's log output.
    #[clap(long, conflicts_with = "emit_bunyan")]
    pub disable_ansi: bool,

    /// Emit Bunyan-formatted logs.
    #[clap(long)]
    pub emit_bunyan: bool,
}

#[derive(Args, Debug)]
#[clap(verbatim_doc_comment)]
pub struct RunOptions {
    /// The command to use to launch the Propolis server.
    #[clap(long, value_parser)]
    pub propolis_server_cmd: Utf8PathBuf,

    /// Git branch name to use for the "current" Propolis server artifact for
    /// migration-from-current tests.
    ///
    /// If this argument is provided, PHD will download the latest Propolis
    /// server artifact from Buildomat for the provided branch name, and use it
    /// to test migration from that Propolis version to the Propolis revision
    /// under test.
    ///
    /// This argument conflicts with the `--current-propolis-commit` and
    /// `--current-propolis-cmd` arguments. If none of these arguments are
    /// provided, no "current" Propolis server artifact will be added to the
    /// artifact store, and migration-from-current tests will be skipped.
    #[clap(
        long,
        conflicts_with("current_propolis_commit"),
        conflicts_with("current_propolis_cmd"),
        value_parser
    )]
    current_propolis_branch: Option<String>,

    /// Git commit hash to use for the "current" Propolis server artifact for
    /// migration-from-current tests.
    ///
    /// If this argument is provided, PHD will download the Propolis server
    /// artifact from Buildomat for the provided commit hash, and use it
    /// to test migration from that Propolis version to the Propolis revision
    /// under test.
    ///
    /// This argument conflicts with the `--current-propolis-branch` and
    /// `--current-propolis-cmd` arguments. If none of these arguments are
    /// provided, no "current" Propolis server artifact will be added to the
    /// artifact store, and migration-from-current tests will be skipped.
    #[clap(
        long,
        conflicts_with("current_propolis_branch"),
        conflicts_with("current_propolis_cmd"),
        value_parser
    )]
    current_propolis_commit: Option<artifacts::buildomat::Commit>,

    /// The path of a local command to use as the "current" Propolis server for
    /// migration-from-current tests.
    ///
    /// If this argument is provided, PHD will use the provided command to run
    /// to test migration from that Propolis binary to the Propolis revision
    /// under test.
    ///
    /// This argument conflicts with the `--current-propolis-branch` and
    /// `--current-propolis-commit` arguments. If none of these arguments are
    /// provided, no "current" Propolis server artifact will be added to the
    /// artifact store, and migration-from-current tests will be skipped.
    #[clap(
        long,
        conflicts_with("current_propolis_commit"),
        conflicts_with("current_propolis_branch"),
        value_parser
    )]
    current_propolis_cmd: Option<Utf8PathBuf>,

    /// The path of a local command to use to launch Crucible downstairs
    /// servers.
    ///
    /// This argument conflicts with the `--crucible-downstairs-commit`
    /// argument, which configures PHD to download a Crucible downstairs
    /// artifact from Buildomat. If neither the `--crucible-downstairs-cmd` OR
    /// `--crucible-downstairs-commit` arguments are provided, then PHD will not
    /// run tests that require Crucible.
    #[clap(long, value_parser)]
    crucible_downstairs_cmd: Option<Utf8PathBuf>,

    /// Git revision to use to download Crucible downstairs artifacts from
    /// Buildomat.
    ///
    /// This may either be the string 'auto' or a 40-character Git commit
    /// hash. If this is 'auto', then the Git revision of Crucible is determined
    /// automatically based on the Propolis workspace's Cargo git dependency on
    /// the `crucible` crate (determined when `phd-runner` is built). If an
    /// explicit commit hash is provided, that commit is downloaded from
    /// Buildomat, regardless of which version of the `crucible` crate Propolis
    /// depends on.
    ///
    /// This argument conflicts with the `--crucible-downstairs-cmd`
    /// argument, which configures PHD to use a local command for running
    /// Crucible downstairs servers. If neither the `--crucible-downstairs-cmd`
    /// OR `--crucible-downstairs-commit` arguments are provided, then PHD will
    /// not run tests that require Crucible.
    #[clap(long, conflicts_with("crucible_downstairs_cmd"), value_parser)]
    crucible_downstairs_commit: Option<ArtifactCommit>,

    /// The directory into which to write temporary files (config TOMLs, log
    /// files, etc.) generated during test execution.
    #[clap(long, value_parser)]
    pub tmp_directory: Utf8PathBuf,

    /// The directory in which artifacts (guest OS images, bootroms, etc.)
    /// are to be stored.
    #[clap(long, value_parser)]
    pub artifact_directory: Utf8PathBuf,

    /// If true, direct Propolis servers created by the runner to log to
    /// stdout/stderr handles inherited from the runner.
    ///
    /// Valid options are:
    ///
    /// - file, tmpfile: Log to a temporary file under tmp-directory.
    ///
    /// - stdio: Log to stdout/stderr.
    ///
    /// - null: Don't log anywhere.
    #[clap(long, default_value = "file")]
    pub server_logging_mode: ServerLogMode,

    /// The number of CPUs to assign to the guest in tests where the test is
    /// using the default machine configuration.
    #[clap(long, value_parser, default_value = "2")]
    pub default_guest_cpus: u8,

    /// The amount of memory, in MiB, to assign to the guest in tests where the
    /// test is using the default machine configuration.
    #[clap(long, value_parser, default_value = "512")]
    pub default_guest_memory_mib: u64,

    /// The path to a TOML file describing the artifact store to use for this
    /// run.
    #[clap(long, value_parser)]
    pub artifact_toml_path: Utf8PathBuf,

    /// The default artifact store key to use to load a guest OS image in tests
    /// that do not explicitly specify one.
    #[clap(long, value_parser, default_value = "alpine")]
    pub default_guest_artifact: String,

    /// The default artifact store key to use to load a guest bootrom in tests
    /// that do not explicitly specify one.
    #[clap(long, value_parser, default_value = "ovmf")]
    pub default_bootrom_artifact: String,

    /// Only run tests whose fully-qualified names contain this string.
    /// Can be specified multiple times.
    #[clap(long, value_parser)]
    pub include_filter: Vec<String>,

    /// Only run tests whose fully-qualified names do not contain this
    /// string. Can be specified multiple times.
    #[clap(long, value_parser)]
    pub exclude_filter: Vec<String>,
}

#[derive(Args, Debug)]
#[clap(verbatim_doc_comment)]
pub struct ListOptions {
    /// Only list tests whose fully-qualified names contain this string.
    /// Can be specified multiple times.
    #[clap(long, value_parser)]
    pub include_filter: Vec<String>,

    /// Only list tests whose fully-qualified names do not contain this
    /// string. Can be specified multiple times.
    #[clap(long, value_parser)]
    pub exclude_filter: Vec<String>,
}

#[derive(Debug, Clone)]
enum ArtifactCommit {
    Auto,
    Explicit(artifacts::buildomat::Commit),
}

impl FromStr for ArtifactCommit {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim();

        if s.eq_ignore_ascii_case("auto") {
            return Ok(ArtifactCommit::Auto);
        }

        s.parse().context(
            "Crucible commit must be either 'auto' or a valid Git commit hash",
        ).map(ArtifactCommit::Explicit)
    }
}

impl RunOptions {
    pub fn crucible_downstairs(
        &self,
    ) -> anyhow::Result<Option<CrucibleDownstairsSource>> {
        // If a local crucible-downstairs command was provided on the command
        // line, use that.
        if let Some(cmd) = self.crucible_downstairs_cmd.clone() {
            return Ok(Some(CrucibleDownstairsSource::Local(cmd)));
        }

        match self.crucible_downstairs_commit {
            Some(ArtifactCommit::Explicit(ref commit)) => Ok(Some(
                CrucibleDownstairsSource::BuildomatGitRev(commit.clone()),
            )),
            Some(ArtifactCommit::Auto) => {
                // Otherwise, use the Git revision of the workspace's Cargo git dep on
                // crucible-upstairs, and use the same revision for the downstairs
                // binary artifact.
                //
                // The Git revision of Crucible we depend on is determined when building
                // `phd-runner` by the build script, so that the `phd-runner` binary can
                // be run even after moving it out of the Propolis cargo workspace.
                let commit = env!("PHD_CRUCIBLE_GIT_REV").parse().context(
                "PHD_CRUCIBLE_GIT_REV must be set to a valid Git revision by the build script",
            )?;
                Ok(Some(CrucibleDownstairsSource::BuildomatGitRev(commit)))
            }
            None => Ok(None),
        }
    }

    pub fn current_propolis(&self) -> Option<CurrentPropolisSource<'_>> {
        // If a local command for the "current" propolis artifact was provided,
        // use that.
        if let Some(ref cmd) = self.current_propolis_cmd {
            return Some(CurrentPropolisSource::Local(cmd));
        }

        if let Some(ref branch) = self.current_propolis_branch {
            return Some(CurrentPropolisSource::BuildomatBranch(branch));
        }

        if let Some(ref commit) = self.current_propolis_commit {
            return Some(CurrentPropolisSource::BuildomatGitRev(commit));
        }

        None
    }
}
