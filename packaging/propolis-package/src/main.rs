// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use anyhow::{anyhow, Context, Result};
use camino::Utf8Path;
use omicron_zone_package::config::{self, PackageName};
use omicron_zone_package::package::BuildConfig;
use omicron_zone_package::progress;
use slog::{o, Drain, Logger};
use std::fs::create_dir_all;

const PKG_NAME: PackageName = PackageName::new_const("propolis-server");

struct PrintProgress(slog::Logger);
impl Default for PrintProgress {
    fn default() -> Self {
        let deco = slog_term::PlainDecorator::new(std::io::stdout());
        let drain = slog_term::CompactFormat::new(deco).build();
        Self(Logger::root(std::sync::Mutex::new(drain).fuse(), o!()))
    }
}
impl progress::Progress for PrintProgress {
    fn get_log(&self) -> &Logger {
        &self.0
    }

    fn set_message(&self, msg: std::borrow::Cow<'static, str>) {
        slog::info!(self.0, "package progress"; "msg" => msg.as_ref());
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let cfg = config::parse("packaging/package-manifest.toml")?;

    let output_dir = Utf8Path::new("out");
    create_dir_all(output_dir)?;

    // We only expect a single package so just look it directly
    let pkg = cfg
        .packages
        .get(&PKG_NAME)
        .with_context(|| anyhow!("missing propolis-server package"))?;

    let progress = PrintProgress::default();
    println!("Building {PKG_NAME} package...");
    pkg.create(
        &PKG_NAME,
        output_dir,
        &BuildConfig { progress: &progress, ..Default::default() },
    )
    .await?;

    println!("Done!");
    Ok(())
}
