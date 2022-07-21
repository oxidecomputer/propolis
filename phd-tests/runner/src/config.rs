use std::path::PathBuf;

use clap::Parser;

#[derive(Debug, Parser)]
pub struct Config {
    /// The command to use to launch the Propolis server.
    #[clap(long, value_parser)]
    pub propolis_server_cmd: PathBuf,

    /// The directory into which to write temporary files (config TOMLs, log
    /// files, etc.) generated during test execution.
    #[clap(long, value_parser)]
    pub tmp_directory: PathBuf,

    /// If true, direct Propolis servers created by the runner to log to
    /// stdout/stderr handles inherited from the runner.
    #[clap(long, value_parser, default_value = "false")]
    pub log_server_to_stdio: bool,

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
    #[clap(long, value_parser, default_value = "ovmf")]
    pub default_bootrom_artifact: String,

    /// Optional. The name of a ZFS filesystem object whose mountpoint is the
    /// local artifact root given in the artifact TOML. If specified, this
    /// allows the runner to use ZFS snapshots to restore artifacts between
    /// tests instead of possibly having to re-download them.
    ///
    /// NOTE: To use this option, the user running the test must have delegated
    /// permissions for the following ZFS operations: snapshot, rollback,
    /// create, destroy, mount.
    #[clap(long, value_parser)]
    pub zfs_fs_name: Option<String>,
}

impl Config {
    pub fn get() -> Self {
        Self::parse()
    }
}
