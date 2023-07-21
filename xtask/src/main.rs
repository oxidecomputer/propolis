// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use anyhow::Result;
use clap::{Parser, Subcommand};

mod task_clippy;
mod task_license;
mod util;

#[derive(Parser)]
#[command(name = "cargo xtask", about = "Builder tasks for Propolis")]
struct Args {
    #[command(subcommand)]
    cmd: Cmds,
}

#[derive(Subcommand)]
enum Cmds {
    /// Run suite of clippy checks
    Clippy {
        /// Treat warnings as errors
        #[arg(short, long)]
        strict: bool,
    },
    /// (Crudely) Check for appropriate license headers
    License,
}

fn main() -> Result<()> {
    match Args::parse().cmd {
        Cmds::Clippy { strict } => task_clippy::cmd_clippy(strict),
        Cmds::License => task_license::cmd_license(),
    }
}
