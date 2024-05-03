// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::collections::BTreeSet;
use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};

use anyhow::Result;

use crate::util::*;

fn check_test_names() -> Result<()> {
    let wroot = workspace_root()?;

    let mut cmd = Command::new("cargo");
    let child = cmd
        .args(["test", "--workspace", "--", "--list", "--format=terse"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .current_dir(&wroot)
        .spawn()?;

    let problem_mods = BufReader::new(child.stdout.expect("stdout is present"))
        .lines()
        .map_while(std::result::Result::ok)
        .filter_map(|line| {
            // Look for "<test name>: test"
            let test_name = match line.rsplit_once(": ") {
                Some((p, "test")) => p,
                _ => return None,
            };

            // skip doctests which follow "<path> - <testname> (line <num>)"
            if test_name.contains(" - ") {
                return None;
            }

            // Check for `mod tests` instead of `mod test` as the last component of
            // the test name;
            match test_name.split("::").collect::<Vec<_>>().iter().nth_back(1) {
                Some(&"tests") => {
                    Some(test_name.rsplit_once("::").unwrap().0.to_owned())
                }
                _ => None,
            }
        })
        .collect::<BTreeSet<_>>();

    if !problem_mods.is_empty() {
        eprintln!("The following test module paths should use `mod test` instead of `mod tests`:");
        for path in problem_mods {
            eprintln!("\t{path}");
        }
        Err(anyhow::anyhow!("Unconforming test module names"))
    } else {
        Ok(())
    }
}

pub(crate) fn cmd_style() -> Result<()> {
    check_test_names()
}
