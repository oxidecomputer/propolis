use anyhow::Context;

fn main() -> anyhow::Result<()> {
    set_crucible_git_rev()
        .context("Failed to determine Crucible Git revision")?;

    Ok(())
}

fn set_crucible_git_rev() -> anyhow::Result<()> {
    fn extract_crucible_dep_sha(
        src: &cargo_metadata::Source,
    ) -> anyhow::Result<&str> {
        const CRUCIBLE_REPO: &str = "https://github.com/oxidecomputer/crucible";

        let src = src.repr.strip_prefix("git+").ok_or_else(|| {
            anyhow::anyhow!("Crucible package's source should be from git")
        })?;

        if !src.starts_with(CRUCIBLE_REPO) {
            println!("cargo:warning=expected Crucible package's source to be {CRUCIBLE_REPO:?}, but is {src:?}");
        }

        let rev = src.split("?rev=").nth(1).ok_or_else(|| {
            anyhow::anyhow!("Crucible package's source should have a revision")
        })?;
        let mut parts = rev.split('#');
        let sha = parts.next().ok_or_else(|| {
            anyhow::anyhow!("Crucible package's source should have a revision")
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

    let crucible_src = crucible_pkg.source.as_ref().ok_or_else(|| {
            anyhow::anyhow!("Crucible package should not be a workspace member, and therefore should have source metadata")
        })?;

    let crucible_sha =
        extract_crucible_dep_sha(crucible_src).with_context(|| {
            format!(
                "Failed to extract Crucible source SHA from {crucible_src:?}"
            )
        })?;

    println!("cargo:rustc-env=PHD_CRUCIBLE_GIT_REV={crucible_sha}");

    Ok(())
}
