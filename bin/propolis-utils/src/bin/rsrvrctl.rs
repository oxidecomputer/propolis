// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use bhyve_api::{ReservoirError, VmmCtlFd};
use clap::Parser;

#[derive(clap::Parser, Debug)]
struct Opts {
    #[clap(subcommand)]
    cmd: Command,
}

#[derive(clap::Subcommand, Debug)]
enum Command {
    /// Add to reservoir capacity
    Add {
        /// Size to add (MiB)
        sz: usize,
        /// Chunk size (MiB)
        chunk: Option<usize>,
    },
    /// Remove from reservoir capacity
    Remove {
        /// Size to remove (MiB)
        sz: usize,
        /// Chunk size (MiB)
        chunk: Option<usize>,
    },
    /// Set reservoir capacity
    Set {
        /// Target size (MiB)
        sz: usize,
        /// Chunk size (MiB)
        chunk: Option<usize>,
    },
    /// Query current reservoir information
    Query,
}

const MB: usize = 1024 * 1024;

fn do_resize(
    ctl: &VmmCtlFd,
    sz_bytes: usize,
    chunk_mb: usize,
) -> std::io::Result<()> {
    loop {
        match ctl.reservoir_resize(sz_bytes, chunk_mb * MB) {
            Err(ReservoirError::Interrupted(sz)) => {
                println!("Reservoir size: {}MiB", sz / MB);
            }
            Err(ReservoirError::Io(e)) => return Err(e),
            Ok(_) => return Ok(()),
        }
    }
}

fn main() -> anyhow::Result<()> {
    let opts = Opts::parse();

    let ctl = VmmCtlFd::open()?;

    let size_total =
        |q: bhyve_api::vmm_resv_query| q.vrq_free_sz + q.vrq_alloc_sz;

    match opts.cmd {
        Command::Add { sz, chunk } => {
            let cur_sz = ctl.reservoir_query().map(size_total)?;

            do_resize(
                &ctl,
                cur_sz.saturating_add(sz * MB),
                chunk.unwrap_or(0),
            )?;
        }
        Command::Remove { sz, chunk } => {
            let cur_sz = ctl.reservoir_query().map(size_total)?;

            do_resize(
                &ctl,
                cur_sz.saturating_sub(sz * MB),
                chunk.unwrap_or(0),
            )?;
        }
        Command::Set { sz, chunk } => {
            do_resize(&ctl, sz * MB, chunk.unwrap_or(0))?;
        }
        Command::Query => {
            let sz = ctl.reservoir_query()?;
            println!(
                "Free KiB:\t\t\t{}\n\
                Allocated KiB:\t\t\t{}\n\
                Transient Allocated KiB:\t{}\n\
                Size limit KiB:\t\t\t{}",
                sz.vrq_free_sz / 1024,
                sz.vrq_alloc_sz / 1024,
                sz.vrq_alloc_transient_sz / 1024,
                sz.vrq_limit / 1024
            );
        }
    }

    Ok(())
}
