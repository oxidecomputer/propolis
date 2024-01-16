// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use camino::Utf8PathBuf;
use clap::{Args, Parser, Subcommand};
use phd_framework::server_log_mode::ServerLogMode;

#[derive(Debug, Subcommand)]
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

    /// The command to use to launch Crucible downstairs servers.
    ///
    /// If this is not present, then a Crucible revision will be determined
    /// based on the current Git revision of `propolis`' dependency on the
    /// `crucible` crate.
    #[clap(long, value_parser)]
    crucible_downstairs_cmd: Option<Utf8PathBuf>,

    /// Disable Crucible.
    ///
    /// If this is set, no crucible-downstairs binary will be downloaded, and
    /// tests which use Crucible will be skipped.
    #[clap(long, conflicts_with("crucible_downstairs_cmd"))]
    no_crucible: bool,

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

impl RunOptions {
    pub fn crucible_downstairs(
        &self,
    ) -> anyhow::Result<Option<phd_framework::CrucibleDownstairsSource>> {
        // Crucible tests are disabled.
        if self.no_crucible {
            return Ok(None);
        }

        // If a local crucible-downstairs command was provided on the command
        // line, use that.
        if let Some(cmd) = self.crucible_downstairs_cmd.clone() {
            return Ok(Some(phd_framework::CrucibleDownstairsSource::Local(
                cmd,
            )));
        }

        // Otherwise, use the Git revision of the workspace's Cargo git dep on
        // crucible-upstairs, and use the same revision for the downstairs
        // binary artifact.
        //
        // The Git revision of Crucible we depend on is determined when building
        // `phd-runner` by the build script, so that the `phd-runner` binary can
        // be run even after moving it out of the Propolis cargo workspace.
        let crucible_git_rev = env!("PHD_CRUCIBLE_GIT_REV");
        Ok(Some(phd_framework::CrucibleDownstairsSource::BuildomatGitRev(
            crucible_git_rev.to_string(),
        )))
    }
}
