// Copyright 2022 Oxide Computer Company

use anyhow::Result;
use std::fs::create_dir_all;
use std::path::Path;

#[tokio::main]
async fn main() -> Result<()> {
    let cfg = omicron_zone_package::config::parse("package-manifest.toml")?;

    let output_dir = Path::new("out");
    create_dir_all(&output_dir)?;

    for package in cfg.packages.values() {
        package.create(output_dir).await?;
    }

    Ok(())
}
