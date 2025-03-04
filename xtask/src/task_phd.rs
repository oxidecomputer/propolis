// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use anyhow::Context;
use camino::{Utf8Path, Utf8PathBuf};
use std::{collections::HashMap, fs, process::Command, time};

macro_rules! cargo_log {
    ($tag:literal, $($arg:tt)*) => {
        eprintln!(
            "{:>indent$} {}",
            owo_colors::OwoColorize::if_supports_color(
                &$tag,
                owo_colors::Stream::Stderr,
                |tag| owo_colors::Style::new().bold().green().style(tag),
            ),
            format_args!($($arg)*),
            indent = 12
        )
    }
}

macro_rules! cargo_warn {
    ($($arg:tt)*) => {
        eprintln!(
            "{}{} {}",
            owo_colors::OwoColorize::if_supports_color(
                &"warning",
                owo_colors::Stream::Stderr,
                |tag| owo_colors::Style::new().bold().yellow().style(tag),
            ),
            owo_colors::OwoColorize::if_supports_color(
                &":",
                owo_colors::Stream::Stderr,
                |tag| owo_colors::Style::new().bold().style(tag),
            ),
            format_args!($($arg)*),
        )
    }
}

#[derive(clap::Subcommand, Debug, Clone)]
#[allow(clippy::large_enum_variant)]
pub(crate) enum Cmd {
    /// Run the PHD test suite.
    Run {
        #[clap(flatten)]
        args: RunArgs,
    },

    /// Delete any temporary directories older than one day.
    Tidy,

    /// List PHD tests
    List {
        /// Arguments to pass to `phd-runner list`.
        #[clap(trailing_var_arg = true, allow_hyphen_values = true)]
        phd_args: Vec<String>,
    },

    /// Display `phd-runner`'s help output.
    RunnerHelp {
        /// Arguments to pass to `phd-runner help`.
        #[clap(trailing_var_arg = true, allow_hyphen_values = true)]
        phd_args: Vec<String>,
    },
}

#[derive(clap::Parser, Debug, Clone)]
pub(crate) struct RunArgs {
    /// If set, temporary directories older than one day will not be
    /// deleted.
    #[clap(long)]
    no_tidy: bool,

    /// Arguments to pass to `phd-runner run`.
    ///
    /// If the `--propolis-server-cmd`, `--crucible-downstairs-commit`,
    /// `--base-propolis-branch`, `--artifact-toml-path`, or
    /// `--artifact-directory` arguments are passed here, they will override
    /// `cargo xtask phd`'s default values for those arguments.
    ///
    /// Use `cargo xtask phd runner-help` for details on the arguments passed to
    /// `phd-runner`.
    #[clap(trailing_var_arg = true, allow_hyphen_values = true)]
    phd_args: Vec<String>,

    #[clap(flatten)]
    propolis_args: PropolisArgs,

    #[clap(flatten)]
    artifact_args: ArtifactStoreArgs,

    #[clap(flatten)]
    base_propolis_args: BasePropolisArgs,

    #[clap(flatten)]
    crucible_args: CrucibleArgs,
}

#[derive(Debug, Clone, clap::Parser)]
#[group(id = "propolis", required = false, multiple = false)]
#[command(next_help_heading = "Propolis Selection")]
struct PropolisArgs {
    /// The command to use to launch the Propolis server.
    ///
    /// If this is not present, a Propolis server binary will be built automatically.
    #[clap(long = "propolis-server-cmd", value_parser, value_hint = clap::ValueHint::FilePath)]
    server_cmd: Option<Utf8PathBuf>,

    /// If set, build `propolis-server` in release mode.
    #[clap(long, short = 'r')]
    release: bool,
}

#[derive(Debug, Clone, clap::Parser)]
#[group(id = "base-propolis", required = false, multiple = false)]
#[command(next_help_heading = "Migration Base Propolis Selection")]
struct BasePropolisArgs {
    /// Git branch name to use for the "migration base" Propolis server artifact
    /// for migration-from-base tests.
    ///
    /// If this argument is provided, PHD will download the latest Propolis
    /// server artifact from Buildomat for the provided branch name, and use it
    /// to test migration from that Propolis version to the Propolis revision
    /// under test.
    ///
    /// This argument conflicts with the `--base-propolis-commit`,
    /// `--base-propolis-cmd`, and `--no-base-propolis` arguments. If none of
    /// these arguments are provided, `cargo xtask phd` will automatically pass
    /// `--base-propolis-branch master` to `phd-runner`.
    #[clap(long, value_parser)]
    base_propolis_branch: Option<String>,

    /// Git commit hash to use for the "migration base" Propolis server artifact for
    /// migration from base tests.
    ///
    /// If this argument is provided, PHD will download the Propolis server
    /// artifact from Buildomat for the provided commit hash, and use it
    /// to test migration from that Propolis version to the Propolis revision
    /// under test.
    ///
    /// This argument conflicts with the `--base-propolis-branch`,
    /// `--base-propolis-cmd`, and `--no-base-propolis` arguments. If none of
    /// these arguments are provided, `cargo xtask phd` will automatically pass
    /// `--base-propolis-branch master` to `phd-runner`.
    #[clap(long, value_parser)]
    base_propolis_commit: Option<String>,

    /// The path of a local command to use as the "migration base" Propolis
    /// server for migration-from-base tests.
    ///
    /// If this argument is provided, PHD will use the provided command to run
    /// to test migration from that Propolis binary to the Propolis revision
    /// under test.
    ///
    /// This argument conflicts with the `--base-propolis-branch`,
    /// `--base-propolis-commit`, and `--no-base-propolis` arguments. If none of
    /// these arguments are provided, `cargo xtask phd` will automatically pass
    /// `--base-propolis-branch master` to `phd-runner`.
    #[clap(
        long,
        value_hint = clap::ValueHint::FilePath,
        value_parser
    )]
    base_propolis_cmd: Option<Utf8PathBuf>,

    /// If set, skip migration-from-base tests.
    ///
    /// If this flag is present, `cargo xtask phd` will not pass
    /// `--base-propolis-branch master` to the `phd-runner` command.
    ///
    /// This flag conflicts with the `--base-propolis-branch`,
    /// `--base-propolis-commit`, and `--base-propolis-cmd` arguments. If none
    /// of these arguments are provided, `cargo xtask phd` will automatically
    /// pass `--base-propolis-branch master` to `phd-runner`.
    #[clap(long)]
    no_base_propolis: bool,
}

#[derive(Debug, Clone, clap::Parser)]
#[group(id = "crucible", required = false, multiple = false)]
#[command(next_help_heading = "Crucible Downstairs Selection")]
struct CrucibleArgs {
    /// The path of a local command to use to launch Crucible downstairs
    /// servers.
    ///
    /// This argument conflicts with the `--crucible-downstairs-commit` and
    /// `--no-crucible` arguments. If none of the `--crucible-downstairs-cmd`,
    /// `--crucible-downstairs-commit`, and `--no-crucible` arguments are
    /// provided, then `cargo xtask phd` will pass `--crucible-downstairs-commit
    /// auto` to `phd-runner`.
    #[clap(long, value_parser, value_hint = clap::ValueHint::FilePath)]
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
    /// This argument conflicts with the `--crucible-downstairs-cmd` and
    /// `--no-crucible` arguments. If none of the `--crucible-downstairs-cmd`,
    /// `--crucible-downstairs-commit`, and `--no-crucible` arguments are
    /// provided, then `cargo xtask phd` will pass `--crucible-downstairs-commit
    /// auto` to `phd-runner`.
    #[clap(long, value_parser)]
    crucible_downstairs_commit: Option<String>,

    /// If set, skip Crucible tests.
    ///
    /// If this flag is present, `cargo xtask phd` will not pass
    /// `--crucible-downstairs-commit auto` to the `phd-runner` command.
    ///
    /// This flag conflicts with the `--crucible-downstairs-cmd` and
    /// `--crucible-downstairs-commit` arguments. If none of the
    /// `--crucible-downstairs-cmd`, `--crucible-downstairs-commit`, and
    /// `--no-crucible` arguments are provided, then `cargo xtask phd` will pass
    /// `--crucible-downstairs-commit auto` to `phd-runner`.
    #[clap(long)]
    no_crucible: bool,
}

#[derive(Debug, Clone, clap::Parser)]
#[command(next_help_heading = "Artifact Store Options")]
struct ArtifactStoreArgs {
    /// The path to a TOML file describing the artifact store to use for this
    /// run.
    #[clap(long, value_parser, value_hint = clap::ValueHint::FilePath)]
    artifact_toml_path: Option<Utf8PathBuf>,

    /// The directory in which artifacts (guest OS images, bootroms, etc.)
    /// are to be stored.
    ///
    /// If this argument is not provided, the default artifact store directory
    /// will be created in `target/phd/artifacts`.
    #[clap(long, value_parser)]
    artifact_directory: Option<Utf8PathBuf>,
}

impl Cmd {
    pub(crate) fn run(self) -> anyhow::Result<()> {
        let meta = cargo_metadata::MetadataCommand::new()
            .no_deps()
            .exec()
            .context("Failed to run cargo metadata")?;
        let phd_dir = relativize(&meta.target_directory).join("phd");

        let mut tmp_dir = phd_dir.join("tmp");
        let now = time::SystemTime::now();

        let RunArgs {
            no_tidy,
            propolis_args,
            phd_args,
            artifact_args,
            base_propolis_args,
            crucible_args,
        } = match self {
            Self::Run { args } => args,
            Self::Tidy => {
                cargo_log!("Tidying up", "old temporary directories...");
                delete_old_tmps(tmp_dir, now)?;
                return Ok(());
            }
            Self::List { phd_args } => {
                let phd_runner = build_bin("phd-runner", false, None, None)?;
                let status = run_exit_code(
                    phd_runner.command().arg("list").args(phd_args),
                )?;
                std::process::exit(status);
            }

            Self::RunnerHelp { phd_args } => {
                let phd_runner = build_bin("phd-runner", false, None, None)?;
                let status = run_exit_code(
                    phd_runner.command().arg("help").args(phd_args),
                )?;
                std::process::exit(status);
            }
        };

        let propolis_local_path = match propolis_args.server_cmd {
            Some(cmd) => {
                cargo_log!("Using", "local Propolis server command {cmd}");
                cmd
            }
            None => {
                let mut server_build_env = HashMap::new();
                server_build_env
                    .insert("PHD_BUILD".to_string(), "true".to_string());
                let bin = build_bin(
                    "propolis-server",
                    propolis_args.release,
                    // Some PHD tests specifically cover cases where a component
                    // in the system has encountered an error, so enable
                    // failure-injection. Do not enable `omicron-build` like we
                    // do in Buildomat because someone running `cargo xtask phd`
                    // is not building a propolis-server destined for Omicron.
                    Some("failure-injection"),
                    Some(server_build_env),
                )?;
                let path = bin
                    .path()
                    .try_into()
                    .context("Propolis server path is not UTF-8")?;
                relativize(path).to_path_buf()
            }
        };

        let artifact_dir =
            artifact_args.artifact_directory.unwrap_or_else(|| {
                // if there's no explicitly overridden `artifact_dir` path, use
                // `target/phd/artifacts`.
                phd_dir.join("artifacts")
            });

        mkdir(&artifact_dir, "artifact directory")?;

        let tmp_dir = {
            if no_tidy {
                cargo_log!(
                    "Skipping",
                    "temp directory cleanup; disabled by `--no-tidy`"
                );
            } else {
                delete_old_tmps(&tmp_dir, now)?;
            }
            tmp_dir.push(
                now.duration_since(time::UNIX_EPOCH)
                    .unwrap()
                    .as_secs()
                    .to_string(),
            );
            tmp_dir
        };

        mkdir(&tmp_dir, "temp directory")?;

        let artifacts_toml =
            artifact_args.artifact_toml_path.unwrap_or_else(|| {
                // if there's no explicitly overridden `artifacts.toml` path,
                // determine the default one from the workspace path.
                relativize(&meta.workspace_root)
                    .join("phd-tests")
                    .join("artifacts.toml")
            });

        if artifacts_toml.exists() {
            cargo_log!("Found", "artifacts.toml at `{artifacts_toml}`")
        } else {
            anyhow::bail!("Missing artifacts config `{artifacts_toml}`!");
        }

        let phd_runner = build_bin("phd-runner", false, None, None)?;
        let mut cmd = if cfg!(target_os = "illumos") {
            let mut cmd = Command::new("pfexec");
            cmd.arg(phd_runner.path());
            cmd
        } else {
            // If we're not on Illumos, running the tests probably won't
            // actually work, because there's almost certainly no Bhyve. But,
            // we'll build and run the command anyway, because being able to run
            // on other systems may still be useful for PHD development (e.g.
            // testing changes to artifact management, etc).
            Command::new(phd_runner.path())
        };
        cmd.arg("run")
            .arg("--propolis-server-cmd")
            .arg(&propolis_local_path)
            .arg("--artifact-toml-path")
            .arg(&artifacts_toml)
            .arg("--artifact-directory")
            .arg(&artifact_dir)
            .arg("--tmp-directory")
            .arg(&tmp_dir);
        crucible_args.configure_command(&mut cmd);
        base_propolis_args.configure_command(&mut cmd);
        cmd.args(phd_args);

        let status = run_exit_code(&mut cmd)?;

        std::process::exit(status);
    }
}

impl CrucibleArgs {
    fn configure_command(&self, cmd: &mut Command) {
        if let Some(ref path) = self.crucible_downstairs_cmd {
            cargo_log!("Using", "local Crucible downstairs: {path}");
            cmd.arg("--crucible-downstairs-cmd").arg(path);
        } else if let Some(ref commit) = self.crucible_downstairs_commit {
            cargo_log!("Using", "Crucible downstairs from commit: {commit}");
            cmd.arg("--crucible-downstairs-commit").arg(commit);
        } else if self.no_crucible {
            cargo_log!("Skipping", "Crucible tests");
        } else {
            cmd.arg("--crucible-downstairs-commit").arg("auto");
        }
    }
}

impl BasePropolisArgs {
    fn configure_command(&self, cmd: &mut Command) {
        if let Some(ref path) = self.base_propolis_cmd {
            cargo_log!("Using", "local migration-base Propolis: {path}");
            cmd.arg("--base-propolis-cmd").arg(path);
        } else if let Some(ref commit) = self.base_propolis_commit {
            cargo_log!(
                "Using",
                "migration-base Propolis from commit: {commit}"
            );
            cmd.arg("--base-propolis-commit").arg(commit);
        } else if let Some(ref branch) = self.base_propolis_branch {
            cargo_log!(
                "Using",
                "migration-base Propolis from branch: {branch}"
            );
            cmd.arg("--base-propolis-branch").arg(branch);
        } else if self.no_base_propolis {
            cargo_log!("Skipping", "migration-from-base tests");
        } else {
            cmd.arg("--base-propolis-branch").arg("master");
        }
    }
}

/// Build the binary `name` in debug or release with an optional build
/// environment variables and a list of Cargo features.
///
/// `features` is passed directly to Cargo, and so must be a space or
/// comma-separated list of features to activate.
fn build_bin(
    name: impl AsRef<str>,
    release: bool,
    features: Option<&str>,
    build_env: Option<HashMap<String, String>>,
) -> anyhow::Result<escargot::CargoRun> {
    let name = name.as_ref();
    cargo_log!("Compiling", "{name}");

    let mut cmd =
        escargot::CargoBuild::new().package(name).bin(name).current_target();
    if let Some(features) = features {
        cmd = cmd.features(features);
    }
    if let Some(env) = build_env {
        for (k, v) in env {
            cmd = cmd.env(k, v);
        }
    }
    let profile = if release {
        cmd = cmd.release();
        "release [optimized]"
    } else {
        "dev [unoptimized + debuginfo]"
    };

    let t0 = time::Instant::now();
    let bin = cmd.run().with_context(|| format!("Failed to build {name}"))?;
    let t1 = t0.elapsed();
    cargo_log!("Finished", "{name} {profile} in {:0.2}s", t1.as_secs_f64());
    Ok(bin)
}

fn mkdir(
    path: impl AsRef<Utf8Path>,
    kind: impl std::fmt::Display,
) -> anyhow::Result<()> {
    let path = path.as_ref();
    if !path.exists() {
        std::fs::create_dir_all(path)
            .with_context(|| format!("Failed to create {kind} `{path}`"))?;
        cargo_log!("Created", "{kind} `{path}`");
    } else {
        cargo_log!("Found", "existing {kind} `{path}`");
    }
    Ok(())
}

fn run_exit_code(cmd: &mut Command) -> anyhow::Result<i32> {
    cargo_log!("Running", "{:#?}", PrettyCmd(cmd));
    cmd.status()
        .with_context(|| {
            format!("Failed to execute command {:?}", PrettyCmd(cmd))
        })?
        .code()
        .ok_or_else(|| {
            anyhow::anyhow!(
                "Command {:?} exited without a status code",
                PrettyCmd(cmd)
            )
        })
}

fn delete_old_tmps(
    tmp_dir: impl AsRef<Utf8Path>,
    now: time::SystemTime,
) -> anyhow::Result<()> {
    let tmp_dir = tmp_dir.as_ref();

    if !tmp_dir.exists() {
        return Ok(());
    }

    let mut deleted = 0;
    let mut sz = 0;
    let mut errs = 0;
    for entry in fs::read_dir(tmp_dir)
        .with_context(|| format!("Failed to read `{tmp_dir}`"))?
    {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                errs += 1;
                cargo_warn!("bad dir entry: {e}");
                continue;
            }
        };
        let path = entry.path();
        let meta = match entry.metadata() {
            Ok(e) => e,
            Err(e) => {
                errs += 1;
                cargo_warn!("failed to stat `{}`: {e}", path.display());
                continue;
            }
        };
        let modified = match meta.modified() {
            Ok(a) => a,
            Err(e) => {
                errs += 1;
                cargo_warn!(
                    "couldn't get last modified time for `{}`: {e}",
                    path.display(),
                );
                continue;
            }
        };
        if let Ok(age) = now.duration_since(modified) {
            const DAY_SECS: u64 = 60 * 60 * 24;
            if age.as_secs() > DAY_SECS {
                match fs::remove_dir_all(&path) {
                    Ok(()) => {
                        deleted += 1;
                        sz += meta.len();
                    }
                    Err(e) => {
                        errs += 1;
                        cargo_warn!(
                            "failed to remove `{}`: {e}",
                            path.display(),
                        );
                    }
                }
            }
        }
    }
    fn pluralize_dir(n: u64) -> &'static str {
        if n == 1 {
            "y"
        } else {
            "ies"
        }
    }

    if deleted > 0 {
        cargo_log!(
            "Tidied up",
            "{deleted} old temp director{}, {sz}B total",
            pluralize_dir(deleted)
        );
    }

    anyhow::ensure!(
        errs == 0,
        "{errs} temp director{} could not be tidied up!",
        pluralize_dir(errs)
    );

    Ok(())
}

struct PrettyCmd<'a>(&'a Command);

impl std::fmt::Debug for PrettyCmd<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let &Self(cmd) = self;
        if let Some(path) =
            Utf8Path::from_path(std::path::Path::new(cmd.get_program()))
        {
            write!(f, "{}", relativize(path))?;
        } else {
            write!(f, "{}", cmd.get_program().to_string_lossy())?;
        }
        for arg in cmd.get_args() {
            let arg = arg.to_string_lossy();
            if f.alternate() && arg.starts_with("--") {
                write!(f, " \\\n\t{arg}")?;
            } else {
                write!(f, " {}", arg)?;
            }
        }

        Ok(())
    }
}

fn relativize(path: &Utf8Path) -> &Utf8Path {
    use std::sync::OnceLock;

    static PWD: OnceLock<Utf8PathBuf> = OnceLock::new();
    let pwd = PWD.get_or_init(|| {
        std::env::current_dir()
            .expect("Failed to get current dir")
            .try_into()
            .expect("Current dir is not UTF-8")
    });
    path.strip_prefix(pwd).unwrap_or(path)
}
