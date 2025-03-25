use std::io::Read;
use std::time::{Duration, Instant};

use anyhow::Context;
use tokio::sync::watch;
use zerocopy::{AsBytes, FromBytes, FromZeroes};

#[derive(Debug, Default)]
pub(crate) struct ProcessStats {
    pub(crate) measurement_time: Duration,
    pub(crate) unshared_rss: u64,
    pub(crate) unshared_vss: u64,
    pub(crate) shared_rss: u64,
    pub(crate) shared_vss: u64,
}

impl ProcessStats {
    pub fn new() -> watch::Receiver<ProcessStats> {
        let (tx, rx) = watch::channel::<ProcessStats>(ProcessStats::default());

        tokio::task::spawn(process_stats_task(tx));

        rx
    }
}

/// There are some arrays with size `PRMAPSZ` holding names in `struct prmap`
/// and `struct prxmap`.
const PRMAPSZ: usize = 64;

/// We're going to query our `/proc/<self>/xmap` here, which contains a series o
/// `struct prxmap` records. So, define it here in the same way it's defined in
/// illumos' `common/sys/procfs.h`
#[derive(Copy, Clone, Debug, FromZeroes, AsBytes, FromBytes)]
// Kind of weird to need packed here, but the struct size is 0x
#[repr(C)]
struct prxmap {
    /// virtual address of the mapping
    pr_vaddr: usize,
    /// size of the mapping in bytes
    pr_size: usize,
    /// name in /proc/<pid>/object
    pr_mapname: [u8; PRMAPSZ],
    /// offset into mapped object, if any
    pr_offset: i64,
    /// protection and attribute flags. See procfs.h' `MA_*` flags
    pr_mflags: i32,
    /// Pagesize (bytes) for this mapping
    pr_pagesize: i32,
    /// SysV shmid, -1 if not SysV shared memory
    pr_shmid: i32,
    /// illumos' `struct prxmap` on 64-bit operating systems has implicit
    /// internal padding between `pr_shmid` and `pr_dev`. This should always be
    /// zero.
    // Rust rules are that padding bytes are uninitialized, so transforming
    // `&prxmap` to a `&[u8]` is UB if there is internal padding.
    // Consequently, `AsBytes` and `FromBytes` require no internal padding, so
    // put a small field here to make the "padding" explicit.
    _internal_pad: i32,
    /// st_dev from stat64() of mapped object, or PRNODEV
    pr_dev: i64,
    /// st_ino from stat64() of mapped object, if any
    pr_ino: u64,
    /// pages of resident memory
    pr_rss: isize,
    /// pages of resident anonymous memory
    pr_anon: isize,
    /// pages of locked memory
    pr_locked: isize,
    /// currently unused
    pr_pad: isize,
    /// pagesize of the hat mapping
    pr_hatpagesize: usize,
    /// filler for future expansion
    // illumos uses `ulong_t` here, which should be 32-bit, but
    // sizes only line up if it's u64?
    pr_filler: [u64; 7],
}

const MA_SHARED: i32 = 0x08;

// Computed here just because it's used in a few places in this file.
const PRXMAPSZ: usize = std::mem::size_of::<prxmap>();

impl prxmap {
    fn is_shared(&self) -> bool {
        self.pr_mflags & MA_SHARED == MA_SHARED
    }
}

pub fn process_stats() -> anyhow::Result<ProcessStats> {
    let stats_read_start = Instant::now();

    // Safety: getpid() does not alter any state, or even read mutable state.
    let mypid = unsafe { libc::getpid() };
    let xmap_path = format!("/proc/{}/xmap", mypid);
    let mut xmap_file = std::fs::File::open(&xmap_path)?;

    let mut stats = ProcessStats {
        measurement_time: Duration::ZERO,
        unshared_rss: 0,
        unshared_vss: 0,
        shared_rss: 0,
        shared_vss: 0,
    };

    let mut buf: [prxmap; 256] = [FromZeroes::new_zeroed(); 256];
    loop {
        let nread = xmap_file
            .read(buf.as_bytes_mut())
            .context("reading struct prxmap from file")?;
        if nread == 0 {
            // we've read all maps this go around.
            stats.measurement_time = stats_read_start.elapsed();
            return Ok(stats);
        }

        assert!(nread % PRXMAPSZ == 0);
        let maps_read = nread / PRXMAPSZ;

        for buf in buf[0..maps_read].iter() {
            let map_rss = buf.pr_rss as u64 * buf.pr_pagesize as u64;
            let map_vss = buf.pr_size as u64;
            if buf.is_shared() {
                stats.shared_rss += map_rss;
                stats.shared_vss += map_vss;
            } else {
                stats.unshared_rss += map_rss;
                stats.unshared_vss += map_vss;
            }
        }
    }
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

#[cfg(test)]
mod test {
    use super::prxmap;
    use std::mem::size_of;

    #[test]
    fn prxmap_size() {
        // Double check `prxmap` size against that of structures in
        // /proc/<pid>/xmap..
        assert_eq!(size_of::<prxmap>(), 0xd8);
    }
}
