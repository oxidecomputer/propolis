// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! External xtasks. (extasks?)

use std::ffi::OsString;
use std::os::unix::process::CommandExt;
use std::process::Command;

use anyhow::{Context, Result};
use clap::Parser;

/// Argument parser for external xtasks.
///
/// In general we want all developer tasks to be discoverable simply by running
/// `cargo xtask`, but some development tools end up with a particularly
/// large dependency tree. It's not ideal to have to pay the cost of building
/// our release engineering tooling if all the user wants to do is check for
/// workspace dependency issues.
///
/// `External` provides a pattern for creating xtasks that live in other crates.
/// An external xtask is defined on `crate::Cmds` as a tuple variant containing
/// `External`, which captures all arguments and options (even `--help`) as
/// a `Vec<OsString>`. The main function then calls `External::exec` with the
/// appropriate bin target name and any additional Cargo arguments.
#[derive(Parser)]
#[clap(
    disable_help_flag(true),
    disable_help_subcommand(true),
    disable_version_flag(true)
)]
pub struct External {
    #[clap(trailing_var_arg(true), allow_hyphen_values(true))]
    args: Vec<OsString>,

    // This stores an in-progress Command builder. `cargo_args` appends args
    // to it, and `exec` consumes it. Clap does not treat this as a command
    // (`skip`), but fills in this field by calling `new_command`.
    #[clap(skip = new_command())]
    command: Command,
}

impl External {
    pub fn exec_bin(
        self,
        package: impl AsRef<str>,
        bin_target: impl AsRef<str>,
    ) -> Result<()> {
        self.exec_common(&[
            "--package",
            package.as_ref(),
            "--bin",
            bin_target.as_ref(),
        ])
    }

    fn exec_common(mut self, args: &[&str]) -> Result<()> {
        let error = self.command.args(args).arg("--").args(self.args).exec();
        Err(error).context("failed to exec `cargo run`")
    }
}

fn new_command() -> Command {
    let mut command = cargo_command(CargoLocation::FromEnv);
    command.arg("run");
    command
}

/// Creates and prepares a `std::process::Command` for the `cargo` executable.
pub fn cargo_command(location: CargoLocation) -> Command {
    let mut command = location.resolve();

    for (key, _) in std::env::vars_os() {
        let Some(key) = key.to_str() else { continue };
        if SANITIZED_ENV_VARS.matches(key) {
            command.env_remove(key);
        }
    }

    command
}

/// How to determine the location of the `cargo` executable.
#[derive(Clone, Copy, Debug)]
pub enum CargoLocation {
    /// Use the `CARGO` environment variable, and fall back to `"cargo"` if it
    /// is not set.
    FromEnv,
}

impl CargoLocation {
    fn resolve(self) -> Command {
        match self {
            CargoLocation::FromEnv => {
                let cargo = std::env::var_os("CARGO")
                    .unwrap_or_else(|| OsString::from("cargo"));
                Command::new(&cargo)
            }
        }
    }
}

#[derive(Debug)]
struct SanitizedEnvVars {
    // At the moment we only ban some prefixes, but we may also want to ban env
    // vars by exact name in the future.
    prefixes: &'static [&'static str],
}

impl SanitizedEnvVars {
    const fn new() -> Self {
        // Remove many of the environment variables set in
        // https://doc.rust-lang.org/cargo/reference/environment-variables.html#environment-variables-cargo-sets-for-build-scripts.
        // This is done to avoid recompilation with crates like ring between
        // `cargo clippy` and `cargo xtask clippy`. (This is really a bug in
        // both ring's build script and in Cargo.)
        //
        // The current list is informed by looking at ring's build script, so
        // it's not guaranteed to be exhaustive and it may need to grow over
        // time.
        let prefixes = &["CARGO_PKG_", "CARGO_MANIFEST_", "CARGO_CFG_"];
        Self { prefixes }
    }

    fn matches(&self, key: &str) -> bool {
        self.prefixes.iter().any(|prefix| key.starts_with(prefix))
    }
}

static SANITIZED_ENV_VARS: SanitizedEnvVars = SanitizedEnvVars::new();
