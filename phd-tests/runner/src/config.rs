use std::path::PathBuf;

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

    /// If true, suppress the emission of terminal control codes in the runner's
    /// log output.
    #[clap(
        long,
        value_parser,
        default_value = "false",
        conflicts_with = "emit-bunyan"
    )]
    pub disable_ansi: bool,

    /// If true, emit Bunyan-formatted logs.
    #[clap(
        long,
        value_parser,
        default_value = "false",
        conflicts_with = "disable-ansi"
    )]
    pub emit_bunyan: bool,
}

#[derive(Args, Debug)]
#[clap(verbatim_doc_comment)]
pub struct RunOptions {
    /// The command to use to launch the Propolis server.
    #[clap(long, value_parser)]
    pub propolis_server_cmd: PathBuf,

    /// The command to use to launch Crucible downstairs servers.
    #[clap(long, value_parser)]
    pub crucible_downstairs_cmd: Option<PathBuf>,

    /// The directory into which to write temporary files (config TOMLs, log
    /// files, etc.) generated during test execution.
    #[clap(long, value_parser)]
    pub tmp_directory: PathBuf,

    /// The directory in which artifacts (guest OS images, bootroms, etc.)
    /// are to be stored.
    #[clap(long, value_parser)]
    pub artifact_directory: PathBuf,

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
    pub artifact_toml_path: PathBuf,

    /// The default artifact store key to use to load a guest OS image in tests
    /// that do not explicitly specify one.
    #[clap(long, value_parser, default_value = "alpine")]
    pub default_guest_artifact: String,

    /// The default artifact store key to use to load a guest bootrom in tests
    /// that do not explicitly specify one.
    #[clap(long, value_parser, default_value = "ovmf_20220922")]
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
