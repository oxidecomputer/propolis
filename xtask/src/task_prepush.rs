// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use anyhow::{bail, Result};

use crate::{task_clippy, task_fmt, task_license, task_style};

pub(crate) fn cmd_prepush(quiet: bool) -> Result<()> {
    let mut errs = Vec::new();
    let checks: [(&str, &dyn Fn() -> bool); 4] = [
        ("clippy", &|| task_clippy::cmd_clippy(true, quiet).is_err()),
        ("fmt", &|| task_fmt::cmd_fmt().is_err()),
        ("license", &|| task_license::cmd_license().is_err()),
        ("style", &|| task_style::cmd_style().is_err()),
    ];

    for (name, func) in checks {
        if !quiet {
            println!("Checking {name}...");
        }
        if func() {
            errs.push(name);
        }
    }

    if !errs.is_empty() {
        bail!("Pre-push error(s) in: {}", errs.join(", "))
    }
    Ok(())
}
