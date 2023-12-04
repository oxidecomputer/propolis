// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use anyhow::{bail, Result};

use crate::{task_clippy, task_fmt, task_license};

pub(crate) fn cmd_prepush() -> Result<()> {
    let mut errs = Vec::new();
    if task_clippy::cmd_clippy(true, true).is_err() {
        errs.push("clippy");
    }
    if task_fmt::cmd_fmt().is_err() {
        errs.push("fmt");
    }
    if task_license::cmd_license().is_err() {
        errs.push("license");
    }

    if !errs.is_empty() {
        bail!("Pre-push error(s) in: {}", errs.join(", "))
    }
    Ok(())
}
