// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use anyhow::Result;
use clap::{Parser, Subcommand};

mod task_clippy;
mod task_fmt;
mod task_license;
mod task_phd;
mod task_prepush;
mod task_style;
mod util;

#[derive(Parser)]
#[command(name = "cargo xtask", about = "Builder tasks for Propolis")]
struct Args {
    #[command(subcommand)]
    cmd: Cmds,
}

#[derive(Subcommand)]
#[allow(clippy::large_enum_variant)]
enum Cmds {
    /// Run suite of clippy checks
    Clippy {
        /// Treat warnings as errors
        #[arg(short, long)]
        strict: bool,

        /// Suppress non-essential output
        #[arg(short, long)]
        quiet: bool,
    },
    /// Check style according to `rustfmt`
    Fmt,
    /// (Crudely) Check for appropriate license headers
    License,
    /// Preform pre-push checks (clippy, license, fmt, etc)
    Prepush,
    /// Run the PHD test suite
    Phd {
        #[clap(subcommand)]
        cmd: task_phd::Cmd,
    },
    /// Perform misc style checks
    Style,
}

fn main() -> Result<()> {
    match Args::parse().cmd {
        Cmds::Clippy { strict, quiet } => {
            task_clippy::cmd_clippy(strict, quiet)
        }
        Cmds::Fmt => task_fmt::cmd_fmt(),
        Cmds::License => {
            task_license::cmd_license()?;

            println!("License checks pass");
            Ok(())
        }
        Cmds::Phd { cmd } => cmd.run(),
        Cmds::Prepush => {
            task_prepush::cmd_prepush()?;

            println!("Pre-push checks pass");
            Ok(())
        }
        Cmds::Style => {
            task_style::cmd_style()?;

            println!("Style checks pass");
            Ok(())
        }
    }
}
