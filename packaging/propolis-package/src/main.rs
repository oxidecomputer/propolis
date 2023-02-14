// Copyright 2022 Oxide Computer Company

use anyhow::anyhow;
use anyhow::Context;
use anyhow::Result;
use indicatif::MultiProgress;
use indicatif::ProgressBar;
use indicatif::ProgressStyle;
use omicron_zone_package::progress::Progress;
use std::fs::create_dir_all;
use std::path::Path;
use std::time::Duration;

const PKG_NAME: &'static str = "propolis-server";

fn in_progress_style() -> ProgressStyle {
    ProgressStyle::default_bar()
        .template(
            "[{elapsed_precise}] {bar:40.cyan/blue} {pos:>7}/{len:7} {msg}",
        )
        .expect("Invalid template")
        .progress_chars("#>.")
}

fn completed_progress_style() -> ProgressStyle {
    ProgressStyle::default_bar()
        .template("[{elapsed_precise}] {bar:40.cyan/blue} {pos:>7}/{len:7} {msg:.green}")
        .expect("Invalid template")
        .progress_chars("#>.")
}

/// Progress UI for the package creation.
struct ProgressUI {
    /// Shared handle for managing all progress bars.
    multi: MultiProgress,

    /// Progress bar for the overall package creation.
    pkg_pb: ProgressBar,
}

impl ProgressUI {
    fn new(total: u64) -> Self {
        // Create overall handle for managing all progress bars.
        let multi = MultiProgress::new();

        // We start off with one main progress bar for the overall package
        // creation. Sub-tasks (e.g. blob download) will dynamically create
        // their own progress bars via `Progress::sub_progress()`.
        let pkg_pb = multi.add(ProgressBar::new(total));
        pkg_pb.set_style(in_progress_style());

        // Don't want to appear stuck while copying large files (e.g. blobs)
        // so enable steady ticks to keep the elapsed time moving.
        pkg_pb.enable_steady_tick(Duration::from_millis(500));

        Self { multi, pkg_pb }
    }
}

impl Progress for ProgressUI {
    fn set_message(&self, msg: std::borrow::Cow<'static, str>) {
        self.pkg_pb.set_message(msg);
    }

    fn increment(&self, delta: u64) {
        self.pkg_pb.inc(delta);
    }

    fn sub_progress(&self, total: u64) -> Box<dyn Progress> {
        let sub_pb = self.multi.add(ProgressBar::new(total));
        sub_pb.set_style(in_progress_style());

        Box::new(SubProgress::new(self.multi.clone(), sub_pb))
    }
}

/// Progress UI for a sub-task (e.g. downloading blobs).
struct SubProgress {
    /// Shared handle for managing all progress bars.
    multi: MultiProgress,

    /// Progress bar for this specific sub-task.
    pb: ProgressBar,
}

impl SubProgress {
    fn new(multi: MultiProgress, pb: ProgressBar) -> Self {
        Self { multi, pb }
    }
}

impl Progress for SubProgress {
    fn set_message(&self, msg: std::borrow::Cow<'static, str>) {
        self.pb.set_message(msg);
    }

    fn increment(&self, delta: u64) {
        self.pb.inc(delta);
    }
}

impl Drop for SubProgress {
    fn drop(&mut self) {
        // Sub-task has completed, so remove the progress bar.
        self.multi.remove(&self.pb);
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let cfg =
        omicron_zone_package::config::parse("packaging/package-manifest.toml")?;

    let output_dir = Path::new("out");
    create_dir_all(output_dir)?;

    // We only expect a single package so just look it directly
    let pkg = cfg
        .packages
        .get(PKG_NAME)
        .with_context(|| anyhow!("missing propolis-server package"))?;

    let progress = ProgressUI::new(pkg.get_total_work());

    pkg.create_with_progress(&progress, PKG_NAME, output_dir).await?;

    progress.pkg_pb.set_style(completed_progress_style());
    progress.pkg_pb.finish_with_message(format!(
        "done: {}",
        pkg.get_output_file(PKG_NAME)
    ));

    Ok(())
}
