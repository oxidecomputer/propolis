// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use serde::Deserialize;

use crate::util::*;

#[derive(Deserialize, Debug)]
struct LicenseRc {
    header: LicenseHeader,
}
#[derive(Deserialize, Debug)]
struct LicenseHeader {
    license: LicenseEnt,
    paths: Vec<String>,
    #[serde(rename = "paths-ignore")]
    paths_ignore: Option<Vec<String>>,
}
#[derive(Deserialize, Debug)]
struct LicenseEnt {
    content: String,
}

fn commentify(raw: String) -> Vec<String> {
    raw.lines().map(|l| format!("// {}", l)).collect::<Vec<String>>()
}

fn check_file(fp: File, needle: &[String]) -> Result<Option<String>> {
    let mut lines = BufReader::new(fp).lines();
    for (num, left) in needle.iter().enumerate() {
        if let Some(right) = lines.next() {
            let right = right?;
            if left == &right {
                continue;
            }
        }
        return Ok(Some(format!(
            "Expected license header not found at line {}",
            num + 1
        )));
    }
    Ok(None)
}

pub(crate) fn cmd_license() -> Result<()> {
    // Find licenserc file
    let ws_root = PathBuf::from(workspace_root()?);
    let mut lic_path = ws_root.clone();
    lic_path.push(".licenserc.yaml");

    let lic_fp =
        File::open(lic_path.as_path()).context("cannot open licenserc file")?;

    let config: LicenseRc = serde_yaml::from_reader(&lic_fp)
        .context("could not parse licenserc file")?;

    if config.header.paths.is_empty() {
        bail!("No file paths configured")
    }
    let ignore = match config.header.paths_ignore.as_ref() {
        Some(paths) => {
            let mut builder = globset::GlobSetBuilder::new();
            for path in paths.iter() {
                builder.add(globset::Glob::new(path).context(format!(
                    "'{}' is not valid path-ignore glob",
                    path
                ))?);
            }
            Some(builder.build()?)
        }
        None => None,
    };

    let needle = commentify(config.header.license.content);

    let mut outcome = true;
    for item in config.header.paths.iter() {
        let mut glob_path = ws_root.clone();
        glob_path.push(item);
        let entries = glob::glob(
            glob_path.to_str().context("glob path not utf-8 valid")?,
        )?;

        for entry in entries {
            let item_path = entry.context("item path not readable")?;
            let item_short_path = item_path.strip_prefix(&ws_root)?;

            if let Some(iglob) = ignore.as_ref() {
                if iglob.is_match(item_short_path) {
                    continue;
                }
            }
            let item_fp = File::open(item_path.as_path())
                .context("could not open item for reading")?;
            let result = check_file(item_fp, &needle)?;
            if let Some(err) = result {
                eprintln!("{}: {}", item_short_path.display(), err);
                outcome = false;
            }
        }
    }
    if !outcome {
        bail!("License errors detected")
    }

    println!("License checks happy!");
    Ok(())
}
