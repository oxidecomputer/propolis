// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::fs::{metadata, File, OpenOptions};
use std::io::{Error, ErrorKind, Result};
use std::num::NonZeroUsize;
use std::os::unix::io::AsRawFd;
use std::path::Path;
use std::sync::{Arc, Mutex};

use crate::block::{self, SyncWorkerCtx, WorkerId};
use crate::tasks::ThreadGroup;
use crate::vmm::{MappingExt, MemCtx};

use anyhow::Context;

pub struct FileBackend {
    state: Arc<SharedState>,
    block_attach: block::BackendAttachment,

    worker_count: NonZeroUsize,
    workers: ThreadGroup,
}
struct SharedState {
    fp: File,

    /// Write-Cache-Enable state (if supported) of the underlying device
    wce_state: Mutex<Option<WceState>>,
    discard_mech: Option<dkioc::DiscardMech>,

    info: block::DeviceInfo,
    skip_flush: bool,
}
struct WceState {
    initial: bool,
    current: bool,
}
impl SharedState {
    fn new(
        fp: File,
        info: block::DeviceInfo,
        skip_flush: bool,
        wce_state: Option<WceState>,
        discard_mech: Option<dkioc::DiscardMech>,
    ) -> Arc<Self> {
        let state = SharedState {
            fp,
            wce_state: Mutex::new(wce_state),
            discard_mech,
            skip_flush,
            info,
        };

        // Attempt to enable write caching if underlying resource supports it
        state.set_wce(true);

        Arc::new(state)
    }

    fn processing_loop(&self, wctx: SyncWorkerCtx) {
        while let Some(dreq) = wctx.block_for_req() {
            let req = dreq.req();
            if self.info.read_only && req.op.is_write() {
                dreq.complete(block::Result::ReadOnly);
                continue;
            }
            if self.discard_mech.is_none() && req.op.is_discard() {
                dreq.complete(block::Result::Unsupported);
                continue;
            }

            let Some(mem) = wctx.acc_mem().access() else {
                dreq.complete(block::Result::Failure);
                continue;
            };
            let res = match self.process_request(&req, &mem) {
                Ok(_) => block::Result::Success,
                Err(_) => block::Result::Failure,
            };
            dreq.complete(res);
        }
    }

    fn process_request(
        &self,
        req: &block::Request,
        mem: &MemCtx,
    ) -> std::result::Result<(), &'static str> {
        match req.op {
            block::Operation::Read(off, len) => {
                let maps = req.mappings(mem).ok_or("mapping unavailable")?;

                let nbytes = maps
                    .preadv(self.fp.as_raw_fd(), off as i64)
                    .map_err(|_| "io error")?;
                if nbytes != len {
                    return Err("bad read length");
                }
            }
            block::Operation::Write(off, len) => {
                let maps = req.mappings(mem).ok_or("bad guest region")?;

                let nbytes = maps
                    .pwritev(self.fp.as_raw_fd(), off as i64)
                    .map_err(|_| "io error")?;
                if nbytes != len {
                    return Err("bad write length");
                }
            }
            block::Operation::Flush => {
                if !self.skip_flush {
                    self.fp.sync_data().map_err(|_| "io error")?;
                }
            }
            block::Operation::Discard(off, len) => {
                if let Some(mech) = self.discard_mech {
                    dkioc::do_discard(&self.fp, mech, off as u64, len as u64)
                        .map_err(|_| {
                        "io error while attempting to free block(s)"
                    })?;
                } else {
                    unreachable!("handled above in processing_loop()");
                }
            }
        }
        Ok(())
    }

    fn set_wce(&self, enabled: bool) {
        if self.info.read_only {
            // Do not needlessly toggle the cache on a read-only disk
            return;
        }
        if let Some(state) = self.wce_state.lock().unwrap().as_mut() {
            if state.current != enabled {
                if let Some(new_wce) = dkioc::set_wce(&self.fp, enabled).ok() {
                    state.current = new_wce;
                }
            }
        }
    }
}
impl Drop for SharedState {
    fn drop(&mut self) {
        // Attempt to return WCE state on the device to how it was when we
        // initially opened it.
        if let Some(state) = self.wce_state.get_mut().unwrap().as_mut() {
            if state.current != state.initial {
                let _ = dkioc::set_wce(&self.fp, state.initial);
            }
        }
    }
}

impl FileBackend {
    /// Creates a new block device from a device at `path`.
    pub fn create(
        path: impl AsRef<Path>,
        opts: block::BackendOpts,
        worker_count: NonZeroUsize,
    ) -> Result<Arc<Self>> {
        let p: &Path = path.as_ref();

        let meta = metadata(p)?;
        let read_only = match (opts.read_only, meta.permissions().readonly()) {
            (Some(false), true) => Err(Error::new(
                ErrorKind::Other,
                "writeable backend with read-only file not allowed",
            )),
            (Some(ro), false) => Ok(ro),
            (_, file_ro) => Ok(file_ro),
        }?;

        let fp = OpenOptions::new().read(true).write(!read_only).open(p)?;
        let len = fp.metadata().unwrap().len();
        let disk_info = dkioc::disk_info(&fp);

        // Do not use the device-queried block size for now. Guests get upset if
        // this changes, and it is likely differen than the old default of 512B
        let block_size = opts.block_size.unwrap_or(block::DEFAULT_BLOCK_SIZE);

        let info = block::DeviceInfo {
            block_size,
            total_size: len / u64::from(block_size),
            read_only,
            supports_discard: disk_info.discard_mech.is_some(),
        };
        let skip_flush = opts.skip_flush.unwrap_or(false);
        let wce_state = if !read_only {
            disk_info
                .wce_state
                .map(|initial| WceState { initial, current: initial })
        } else {
            None
        };
        let block_attach = block::BackendAttachment::new(worker_count, info);
        Ok(Arc::new(Self {
            state: SharedState::new(
                fp,
                info,
                skip_flush,
                wce_state,
                disk_info.discard_mech,
            ),
            block_attach,
            worker_count,
            workers: ThreadGroup::new(),
        }))
    }
    fn spawn_workers(&self) -> std::io::Result<()> {
        let spawn_results = (0..self.worker_count.get())
            .map(|n| {
                let shared_state = self.state.clone();
                let wctx = self.block_attach.worker(n as WorkerId);

                std::thread::Builder::new()
                    .name(format!("file worker {n}"))
                    .spawn(move || {
                        let wctx = wctx
                            .activate_sync()
                            .expect("worker slot is uncontended");
                        shared_state.processing_loop(wctx);
                    })
            })
            .collect::<Vec<_>>();

        self.workers.extend(spawn_results.into_iter())
    }
}

#[async_trait::async_trait]
impl block::Backend for FileBackend {
    fn attachment(&self) -> &block::BackendAttachment {
        &self.block_attach
    }

    async fn start(&self) -> anyhow::Result<()> {
        self.block_attach.start();
        if let Err(e) = self.spawn_workers() {
            self.block_attach.stop();
            self.workers.block_until_joined();
            Err(e).context("failure while spawning workers")
        } else {
            Ok(())
        }
    }

    async fn stop(&self) -> () {
        self.block_attach.stop();
        self.workers.block_until_joined();
    }
}

mod dkioc {
    #![allow(non_camel_case_types)]

    use std::fs::File;
    use std::io::Result;
    use std::os::raw::{c_int, c_longlong, c_uint};
    use std::os::unix::fs::FileTypeExt;
    use std::os::unix::io::AsRawFd;

    use crate::block::DEFAULT_BLOCK_SIZE;
    use crate::util::ioctl;

    const DKIOC: i32 = 0x04 << 8;
    const DKIOCGETWCE: i32 = DKIOC | 36;
    const DKIOCSETWCE: i32 = DKIOC | 37;
    const DKIOCGMEDIAINFOEXT: i32 = DKIOC | 48;
    const DKIOCFREE: i32 = DKIOC | 50;
    const DKIOC_CANFREE: i32 = DKIOC | 60;

    #[derive(Copy, Clone)]
    pub(crate) enum DiscardMech {
        /// Discard via `ioctl(DKIOCFREE)`
        DkiocFree,
        /// Discard via `fcntl(F_FREESP)`
        FnctlFreesp,
    }

    #[derive(Copy, Clone)]
    #[allow(unused, dead_code)]
    pub(crate) struct DiskInfo {
        /// WCE state (if any) for disk
        ///
        /// Block devices (including "real" disks and zvols) will regard all writes as
        /// synchronous when performed via the /dev/rdsk endpoint when WCE is not
        /// enabled.  With WCE enabled, writes can be cached on the device, to be
        /// flushed later via fsync().
        pub wce_state: Option<bool>,
        /// Block size of disk
        pub block_size: u32,
        /// Does the disk support use of DKIOCFREE or F_FREESP?
        pub discard_mech: Option<DiscardMech>,
    }
    impl Default for DiskInfo {
        fn default() -> Self {
            Self {
                wce_state: None,
                block_size: DEFAULT_BLOCK_SIZE,
                discard_mech: None,
            }
        }
    }

    pub(crate) fn disk_info(fp: &File) -> DiskInfo {
        match fp.metadata() {
            Ok(ft) if ft.file_type().is_char_device() => {
                // Continue on to attempt DKIOC lookups on raw disk devices
            }
            Ok(ft) if ft.file_type().is_file() => {
                // Assume fcntl(F_FREESP) support for files
                return DiskInfo {
                    discard_mech: Some(DiscardMech::FnctlFreesp),
                    ..Default::default()
                };
            }
            _ => {
                return DiskInfo::default();
            }
        }

        let wce_state = unsafe {
            let mut res: c_int = 0;

            ioctl(fp.as_raw_fd(), DKIOCGETWCE, &mut res as *mut c_int as _)
                .ok()
                .map(|_| res != 0)
        };

        let can_free = unsafe {
            let mut res: c_int = 0;
            ioctl(fp.as_raw_fd(), DKIOC_CANFREE, &mut res as *mut c_int as _)
                .ok()
                .map(|_| res != 0)
                .unwrap_or(false)
        };

        let block_size = unsafe {
            let mut info = dk_minfo_ext::default();
            ioctl(
                fp.as_raw_fd(),
                DKIOCGMEDIAINFOEXT,
                &mut info as *mut dk_minfo_ext as _,
            )
            .ok()
            .map(|_| info.dki_pbsize)
            .unwrap_or(DEFAULT_BLOCK_SIZE)
        };

        DiskInfo {
            wce_state,
            block_size,
            discard_mech: can_free.then_some(DiscardMech::DkiocFree),
        }
    }

    /// Attempt to set the Write-Cache-Enable state for a given open device
    pub(crate) fn set_wce(fp: &File, enabled: bool) -> Result<bool> {
        let mut flag: c_int = enabled.into();
        unsafe {
            ioctl(fp.as_raw_fd(), DKIOCSETWCE, &mut flag as *mut c_int as _)
                .map(|_| enabled)
        }
    }

    pub(crate) fn do_discard(
        fp: &File,
        mech: DiscardMech,
        off: u64,
        len: u64,
    ) -> Result<()> {
        match mech {
            DiscardMech::DkiocFree => {
                let mut req = dkioc_free_list {
                    dfl_flags: 0,
                    dfl_num_exts: 1,
                    dfl_offset: 0,
                    dfl_exts: [dkioc_free_list_ext {
                        dfle_start: off,
                        dfle_length: len,
                    }],
                };
                unsafe {
                    ioctl(
                        fp.as_raw_fd(),
                        DKIOCFREE,
                        &mut req as *mut dkioc_free_list as _,
                    )?;
                };
                Ok(())
            }
            DiscardMech::FnctlFreesp => {
                // If the target platform doesn't define F_WRLCK to be a type
                // quivalent to i16, we have to cast it. But if it's already an
                // i16 the cast is linted for being pointless. cfg() it to only
                // exist when needed.
                #[cfg(target_os = "linux")]
                let l_type = libc::F_WRLCK as i16;
                #[cfg(not(target_os = "linux"))]
                let l_type = libc::F_WRLCK;

                let mut fl = libc::flock {
                    l_type,
                    l_whence: 0,
                    l_start: off as i64,
                    l_len: len as i64,
                    // Ugly hack to zero out struct members we do not care about
                    ..unsafe { std::mem::MaybeUninit::zeroed().assume_init() }
                };

                // Make this buildable on non-illumos, despite the F_FREESP
                // fnctl command being unavailable elsewhere.
                #[cfg(target_os = "illumos")]
                let fcntl_cmd = libc::F_FREESP;
                #[cfg(not(target_os = "illumos"))]
                let fcntl_cmd = -1;

                let res = unsafe {
                    libc::fcntl(
                        fp.as_raw_fd(),
                        fcntl_cmd,
                        &mut fl as *mut libc::flock as *mut libc::c_void,
                    )
                };
                if res != 0 {
                    Err(std::io::Error::last_os_error())
                } else {
                    Ok(())
                }
            }
        }
    }

    #[derive(Copy, Clone, Default)]
    #[repr(C)]
    struct dkioc_free_list_ext {
        dfle_start: u64,
        dfle_length: u64,
    }

    #[derive(Copy, Clone, Default)]
    #[repr(C)]
    struct dkioc_free_list {
        dfl_flags: u64,
        dfl_num_exts: u64,
        dfl_offset: u64,
        dfl_exts: [dkioc_free_list_ext; 1],
    }

    #[derive(Copy, Clone, Default)]
    #[repr(C)]
    struct dk_minfo_ext {
        dki_media_type: c_uint,
        dki_lbsize: c_uint,
        dki_capacity: c_longlong,
        dki_pbsize: c_uint,
    }
}
