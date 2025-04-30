// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::io::Read;
use std::time::{Duration, Instant};

use anyhow::Context;
use tokio::sync::watch;
use zerocopy::{AsBytes, FromBytes, FromZeroes};

#[derive(Debug, Default)]
pub(crate) struct ProcessStats {
    pub(crate) measurement_time: Duration,
    pub(crate) rss: usize,
    pub(crate) vss: usize,
}

impl ProcessStats {
    pub fn new() -> watch::Receiver<ProcessStats> {
        let (tx, rx) = watch::channel::<ProcessStats>(ProcessStats::default());

        tokio::task::spawn(process_stats_task(tx));

        rx
    }
}

const PRFNSZ: usize = 16;
const PRARGSZ: usize = 80;

/// From `sys/types.h`
#[allow(non_camel_case_types)]
type taskid_t = i32;
/// From `sys/time_impl.h`
#[allow(non_camel_case_types)]
type timespec_t = [i64; 2];
/// From `sys/time_impl.h`
#[allow(non_camel_case_types)]
type timestruc_t = timespec_t;
/// From `sys/types.h`
#[allow(non_camel_case_types)]
type dev_t = u64;

/// `psinfo`'s definition depends on the data model reported by illumos. This is
/// in line with the 64-bit version of the struct, and ignores a few fields at
/// the end of the struct.
#[derive(Copy, Clone, Debug, FromZeroes, AsBytes, FromBytes)]
#[cfg(target_arch = "x86_64")]
#[repr(C)]
struct psinfo {
    pr_flag: i32,
    pr_nlwp: i32,
    pr_pid: u32,
    pr_ppid: u32,
    pr_pgid: u32,
    pr_sid: u32,
    pr_uid: u32,
    pr_euid: u32,
    pr_gid: u32,
    pr_egid: u32,
    pr_addr: usize,
    pr_size: usize,
    pr_rssize: usize,
    // From `struct psinfo`. This seems to be present to ensure that `pr_ttydev`
    // is 64-bit aligned even on 32-bit targets.
    pr_pad1: usize,
    pr_ttydev: dev_t,
    pr_pctcpu: u16,
    pr_pctmem: u16,
    // This padding is not explicitly present in illumos' `struct procfs`, but
    // is none the less there due to C struct layout rules.
    _pad2: u32,
    pr_start: timestruc_t,
    pr_time: timestruc_t,
    pr_ctime: timestruc_t,
    pr_fname: [u8; PRFNSZ],
    pr_psargs: [u8; PRARGSZ],
    pr_wstat: u32,
    pr_argc: u32,
    pr_argv: usize,
    pr_envp: usize,
    pr_dmodel: u8,
    // More padding from `struct psinfo`.
    pr_pad2: [u8; 3],
    pr_taskid: taskid_t,
}

pub fn process_stats() -> anyhow::Result<ProcessStats> {
    let mut psinfo_file = std::fs::File::open("/proc/self/psinfo")?;

    let mut stats =
        ProcessStats { measurement_time: Duration::ZERO, vss: 0, rss: 0 };

    let mut info: psinfo = FromZeroes::new_zeroed();

    let stats_read_start = Instant::now();

    psinfo_file
        .read(info.as_bytes_mut())
        .context("reading struct psinfo from file")?;

    stats.measurement_time = stats_read_start.elapsed();

    stats.vss = info.pr_size;
    stats.rss = info.pr_rssize;

    Ok(stats)
}

async fn process_stats_task(tx: watch::Sender<ProcessStats>) {
    while !tx.is_closed() {
        let new_process_stats = tokio::task::spawn_blocking(process_stats)
            .await
            .expect("collecting address space stats does not panic")
            .expect("process_stats() does not error");

        // [there isn't a nice way to plumb a slog Logger here, so this is an
        // eprintln for demonstrative purposes but otherwise should be removed.]
        eprintln!(
            "Sending new stats at least.. {:?}, took {}ms",
            new_process_stats,
            new_process_stats.measurement_time.as_millis()
        );
        tx.send_replace(new_process_stats);

        tokio::time::sleep(std::time::Duration::from_millis(15000)).await;
    }
}
