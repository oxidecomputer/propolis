// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::process::Command;

use anyhow::{bail, Result};

use crate::util::*;

pub(crate) fn cmd_fmt() -> Result<()> {
    let wroot = workspace_root()?;

    let mut cmd = Command::new("cargo");
    cmd.arg("fmt").arg("--check").current_dir(&wroot);

    if !cmd.spawn()?.wait()?.success() {
        bail!("rustfmt failure(s) detected")
    }

    Ok(())
}
