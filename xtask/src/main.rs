// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::process::{Command, Stdio};

use anyhow::{bail, Result};
use clap::{Parser, Subcommand};
use serde_json::{Map, Value};

#[derive(Parser)]
#[command(name = "cargo xtask", about = "Builder tasks for Propolis")]
struct Args {
    #[command(subcommand)]
    cmd: Cmds,
}

#[derive(Subcommand)]
enum Cmds {
    /// Run suite of clippy checks
    Clippy {
        /// Treat warnings as errors
        #[arg(short, long)]
        strict: bool,
    },
}

fn workspace_root() -> Result<String> {
    let mut cmd = Command::new("cargo");
    cmd.args(["metadata", "--format-version=1"])
        .stdin(Stdio::null())
        .stderr(Stdio::inherit());

    let output = cmd.output()?;
    if !output.status.success() {
        bail!("failed to query cargo metadata");
    }
    let metadata: Map<String, Value> = serde_json::from_slice(&output.stdout)?;

    if let Some(Value::String(root)) = metadata.get("workspace_root") {
        Ok(root.clone())
    } else {
        bail!("could not location workspace root")
    }
}

fn cmd_clippy(strict: bool) -> Result<()> {
    let wroot = workspace_root()?;

    let run_clippy = |args: &[&str]| -> Result<bool> {
        let mut cmd = Command::new("cargo");
        cmd.arg("clippy").arg("--no-deps").args(args).current_dir(&wroot);

        if strict {
            cmd.args(["--", "-Dwarnings"]);
        }

        let status = cmd.spawn()?.wait()?;
        Ok(!status.success())
    };

    let mut failed = false;

    // Everything in the workspace (including tests, etc)
    failed |= run_clippy(&["--workspace", "--all-targets"])?;

    // Check the server as it is built for production
    failed |=
        run_clippy(&["-p", "propolis-server", "--features", "omicron-build"])?;

    // Check the Falcon bits
    failed |= run_clippy(&["-p", "propolis-server", "--features", "falcon"])?;

    // Check the mock server
    failed |=
        run_clippy(&["-p", "propolis-server", "--features", "mock-only"])?;

    if failed {
        bail!("Clippy failures detected")
    }

    Ok(())
}

fn main() -> Result<()> {
    match Args::parse().cmd {
        Cmds::Clippy { strict } => cmd_clippy(strict),
    }
}
