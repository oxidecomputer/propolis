// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use anyhow::Context;
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

        // Otherwise, determine the Git SHA of the workspace's Cargo git dep on
        // crucible-upstairs, and use the same revision for the downstairs
        // binary artifact.
        fn extract_crucible_dep_sha(
            src: &cargo_metadata::Source,
        ) -> anyhow::Result<&str> {
            const CRUCIBLE_REPO: &str =
                "https://github.com/oxidecomputer/crucible";

            let src = src.repr.strip_prefix("git+").ok_or_else(|| {
                anyhow::anyhow!("Crucible package's source should be from git")
            })?;

            if !src.starts_with(CRUCIBLE_REPO) {
                println!("cargo:warning=expected Crucible package's source to be {CRUCIBLE_REPO:?}, but is {src:?}");
            }

            let rev = src.split("?rev=").nth(1).ok_or_else(|| {
                anyhow::anyhow!(
                    "Crucible package's source should have a revision"
                )
            })?;
            let mut parts = rev.split('#');
            let sha = parts.next().ok_or_else(|| {
                anyhow::anyhow!(
                    "Crucible package's source should have a revision"
                )
            })?;
            assert_eq!(Some(sha), parts.next());
            Ok(sha)
        }

        let metadata = cargo_metadata::MetadataCommand::new()
            .exec()
            .context("Failed to get cargo metadata")?;

        let crucible_pkg = metadata
            .packages
            .iter()
            .find(|pkg| pkg.name == "crucible")
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "Failed to find Crucible package in cargo metadata"
                )
            })?;

        let crucible_src = crucible_pkg.source.as_ref().ok_or_else(|| {
                anyhow::anyhow!("Crucible package should not be a workspace member, and therefore should have source metadata")
            })?;

        let crucible_sha =
            extract_crucible_dep_sha(crucible_src).with_context(|| {
                format!(
                    "Failed to extract Crucible source SHA from {crucible_src:?}"
                )
            })?;

        Ok(Some(phd_framework::CrucibleDownstairsSource::BuildomatGitRev(
            crucible_sha.to_string(),
        )))
    }
}
