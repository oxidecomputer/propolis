// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use anyhow::Context;
use camino::{Utf8Path, Utf8PathBuf};
use std::{env, fs, process::Command, time};

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

pub(crate) fn cmd_phd(phd_args: Vec<String>) -> anyhow::Result<()> {
    let phd_runner = build_bin("phd-runner")?;

    let mut arg_iter = phd_args.iter().map(String::as_str);
    // If the `phd-runner` subcommand is not `run`, just forward straight to
    // phd-runner without any extra processing.
    if arg_iter.next() != Some("run") {
        let status = run_exit_code(phd_runner.command().args(phd_args))?;
        std::process::exit(status);
    }

    // Bash-script-style arg parsing, rather than using `clap`, because we
    // want to filter out the args we default regardless of their position in
    // the input. A `clap` parser can only accept unrecognized args if they're
    // trailing after all recognized args, which isn't the behavior we want, as
    // we don't know what order the `phd-runner` command line will come in.
    let mut bonus_args = Vec::new();
    let mut propolis_base_branch = None;
    let mut propolis_local_path = None;
    let mut crucible_commit = None;
    let mut artifacts_toml = None;
    while let Some(arg) = arg_iter.next() {
        macro_rules! args {
            ($($arg:path => $var:ident),+$(,)?) => {
                match arg.as_ref() {
                    $(
                        $arg => {
                            let val = arg_iter.next().ok_or_else(|| {
                                anyhow::anyhow!("Missing value for argument `{}`", $arg)
                            })?;
                            cargo_log!("Overridden", "{} {val:?}", $arg);
                            $var = Some(val);
                        }
                    ),+
                    _ => bonus_args.push(arg),
                }
            }
        }
        args! {
            args::PROPOLIS_BASE => propolis_base_branch,
            args::PROPOLIS_CMD => propolis_local_path,
            args::CRUCIBLE_COMMIT => crucible_commit,
            args::ARTIFACTS_TOML => artifacts_toml,
        }
    }

    let propolis_local_path = match propolis_local_path {
        Some(path) => path.into(),
        None => {
            let bin = build_bin("propolis-server")?;
            let path = bin
                .path()
                .try_into()
                .context("Propolis server path is not UTF-8")?;
            relativize(path).to_path_buf()
        }
    };

    let meta = cargo_metadata::MetadataCommand::new()
        .no_deps()
        .exec()
        .context("Failed to run cargo metadata")?;
    let phd_dir = relativize(&meta.target_directory).join("phd");
    let artifact_dir = phd_dir.join("artifacts");
    mkdir(&artifact_dir, "artifact directory")?;

    let tmp_dir = {
        let mut tmp_dir = phd_dir.join("tmp");
        let now = time::SystemTime::now();
        delete_old_tmps(&tmp_dir, now)?;
        tmp_dir.push(
            now.duration_since(time::UNIX_EPOCH).unwrap().as_secs().to_string(),
        );
        tmp_dir
    };

    mkdir(&tmp_dir, "temp directory")?;

    let artifacts_toml =
        artifacts_toml.map(Utf8PathBuf::from).unwrap_or_else(|| {
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

    let status = run_exit_code(
        phd_runner
            .command()
            .arg(args::RUN)
            .arg(args::PROPOLIS_CMD)
            .arg(&propolis_local_path)
            .arg(args::CRUCIBLE_COMMIT)
            .arg(crucible_commit.unwrap_or("auto"))
            .arg(args::PROPOLIS_BASE)
            .arg(propolis_base_branch.unwrap_or("master"))
            .arg(args::ARTIFACTS_TOML)
            .arg(&artifacts_toml)
            .arg("--artifact-directory")
            .arg(&artifact_dir)
            .arg("--tmp-directory")
            .arg(&tmp_dir)
            .args(bonus_args),
    )?;

    std::process::exit(status);
}

mod args {
    pub(super) const RUN: &str = "run";
    pub(super) const PROPOLIS_CMD: &str = "--propolis-server-cmd";
    pub(super) const PROPOLIS_BASE: &str = "--base-propolis-branch";
    pub(super) const CRUCIBLE_COMMIT: &str = "--crucible-downstairs-commit";
    pub(super) const ARTIFACTS_TOML: &str = "--artifact-toml-path";
}

fn build_bin(name: impl AsRef<str>) -> anyhow::Result<escargot::CargoRun> {
    const PROFILE: &str = if cfg!(debug_assertions) {
        "dev [unoptimized + debuginfo]"
    } else {
        "release [optimized]"
    };
    let name = name.as_ref();
    cargo_log!("Compiling", "{name}");

    let t0 = time::Instant::now();
    let bin = escargot::CargoBuild::new()
        .package(name)
        .bin(name)
        .current_release()
        .current_target()
        .run()
        .with_context(|| format!("Failed to build {name}"))?;
    let t1 = t0.elapsed();
    cargo_log!("Finished", "{name} {PROFILE} in {:0.2}s", t1.as_secs_f64());
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
    if env::var_os("PHD_NOTIDY").is_some() {
        cargo_log!(
            "Skipping",
            "temp directory cleanup; disabled by `PHD_NOTIDY`"
        );
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
        let accessed = match meta.accessed() {
            Ok(a) => a,
            Err(e) => {
                errs += 1;
                cargo_warn!(
                    "couldn't get last accessed time for `{}`: {e}",
                    path.display(),
                );
                continue;
            }
        };
        if let Ok(age) = now.duration_since(accessed) {
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
