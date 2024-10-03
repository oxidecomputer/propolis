// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use anyhow::Context;

fn main() -> anyhow::Result<()> {
    set_crucible_git_rev()
        .context("Failed to determine Crucible Git revision")?;

    Ok(())
}

fn set_crucible_git_rev() -> anyhow::Result<()> {
    const CRUCIBLE_REPO: &str = "https://github.com/oxidecomputer/crucible";
    fn extract_crucible_dep_sha(
        src: &cargo_metadata::Source,
    ) -> anyhow::Result<&str> {
        let src = src.repr.strip_prefix("git+").ok_or_else(|| {
            anyhow::anyhow!("Crucible is not a Git dependency")
        })?;

        if !src.starts_with(CRUCIBLE_REPO) {
            println!("cargo:warning=expected Crucible package's source to be {CRUCIBLE_REPO:?}, but is {src:?}");
        }

        let rev = src.split("?rev=").nth(1).ok_or_else(|| {
            anyhow::anyhow!("Crucible package's source did not have a revision")
        })?;
        let mut parts = rev.split('#');
        let sha = parts.next().ok_or_else(|| {
            anyhow::anyhow!("Crucible package's source did not have a revision")
        })?;
        assert_eq!(Some(sha), parts.next());
        Ok(sha)
    }

    let metadata = cargo_metadata::MetadataCommand::new()
        .exec()
        .context("Failed to get cargo metadata")?;

    let crucible_pkg = metadata
        .packages
        .iter()
        .find(|pkg| pkg.name == "crucible")
        .ok_or_else(|| {
            anyhow::anyhow!("Failed to find Crucible package in cargo metadata")
        })?;

    let mut errmsg = String::new();
    let crucible_sha = crucible_pkg
        .source
        .as_ref()
        .ok_or_else(|| {
            anyhow::anyhow!(
                "Crucible dependency is patched with a local checkout"
            )
        })
        .and_then(extract_crucible_dep_sha)
        .unwrap_or_else(|err| {
            println!(
                "cargo:warning={err}, so the `--crucible-downstairs-commit auto` \
                 flag will be disabled in this PHD build",
            );
            errmsg = format!("CANT_GET_YE_CRUCIBLE_SHA{err}");
            &errmsg
        });

    println!("cargo:rustc-env=PHD_CRUCIBLE_GIT_REV={crucible_sha}");

    Ok(())
}
