// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::process::Command;

use anyhow::{bail, Result};

use crate::util::*;

pub(crate) fn cmd_clippy(strict: bool) -> Result<()> {
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
    failed |= run_clippy(&[
        "--features",
        "falcon",
        "-p",
        "propolis-server",
        "-p",
        "propolis-client",
    ])?;

    // Check the mock server
    failed |= run_clippy(&["-p", "propolis-mock-server"])?;

    // Check standalone with crucible enabled
    failed |=
        run_clippy(&["-p", "propolis-standalone", "--features", "crucible"])?;

    // Check PHD bits
    failed |= run_clippy(&["-p", "phd-runner"])?;

    if failed {
        bail!("Clippy failures detected")
    }

    Ok(())
}
