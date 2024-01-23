// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::fs::{metadata, File, OpenOptions};
use std::io::{Error, ErrorKind, Result};
use std::num::NonZeroUsize;
use std::os::raw::c_int;
use std::os::unix::fs::FileTypeExt;
use std::os::unix::io::AsRawFd;
use std::path::Path;
use std::sync::{Arc, Mutex};

use crate::accessors::MemAccessor;
use crate::block::{self, DeviceInfo};
use crate::tasks::ThreadGroup;
use crate::util::ioctl;
use crate::vmm::{MappingExt, MemCtx};

use anyhow::Context;

// XXX: completely arb for now
const MAX_WORKERS: usize = 32;

const DKIOC: i32 = 0x04 << 8;
const DKIOCGETWCE: i32 = DKIOC | 36;
const DKIOCSETWCE: i32 = DKIOC | 37;

pub struct FileBackend {
    state: Arc<WorkerState>,

    worker_count: NonZeroUsize,
    workers: ThreadGroup,
}
struct WorkerState {
    attachment: block::BackendAttachment,
    fp: File,

    /// Write-Cache-Enable state (if supported) of the underlying device
    wce_state: Mutex<Option<WceState>>,

    info: block::DeviceInfo,
    skip_flush: bool,
}
struct WceState {
    initial: bool,
    current: bool,
}
impl WorkerState {
    fn new(fp: File, info: block::DeviceInfo, skip_flush: bool) -> Arc<Self> {
        let wce_state = match info.read_only {
            true => None,
            false => get_wce(&fp)
                .map(|initial| WceState { initial, current: initial }),
        };

        let state = WorkerState {
            attachment: block::BackendAttachment::new(),
            fp,
            wce_state: Mutex::new(wce_state),
            skip_flush,
            info,
        };

        // Attempt to enable write caching if underlying resource supports it
        state.set_wce(true);

        Arc::new(state)
    }

    fn processing_loop(&self, acc_mem: MemAccessor) {
        while let Some(req) = self.attachment.block_for_req() {
            if self.info.read_only && req.oper().is_write() {
                req.complete(block::Result::ReadOnly);
                continue;
            }

            let mem = match acc_mem.access() {
                Some(m) => m,
                None => {
                    req.complete(block::Result::Failure);
                    continue;
                }
            };
            let res = match self.process_request(&req, &mem) {
                Ok(_) => block::Result::Success,
                Err(_) => block::Result::Failure,
            };
            req.complete(res);
        }
    }

    fn process_request(
        &self,
        req: &block::Request,
        mem: &MemCtx,
    ) -> std::result::Result<(), &'static str> {
        match req.oper() {
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
        }
        Ok(())
    }

    fn set_wce(&self, enabled: bool) {
        if let Some(state) = self.wce_state.lock().unwrap().as_mut() {
            if state.current != enabled {
                if let Some(new_wce) = set_wce(&self.fp, enabled).ok() {
                    state.current = new_wce;
                }
            }
        }
    }
}
impl Drop for WorkerState {
    fn drop(&mut self) {
        // Attempt to return WCE state on the device to how it was when we
        // initially opened it.
        if let Some(state) = self.wce_state.get_mut().unwrap().as_mut() {
            if state.current != state.initial {
                let _ = set_wce(&self.fp, state.initial);
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
        if worker_count.get() > MAX_WORKERS {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "too many workers",
            ));
        }
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
        // TODO: attempt to query blocksize from underlying file/zvol
        let block_size = opts.block_size.unwrap_or(block::DEFAULT_BLOCK_SIZE);

        let info = block::DeviceInfo {
            block_size,
            total_size: len / u64::from(block_size),
            read_only,
        };
        let skip_flush = opts.skip_flush.unwrap_or(false);
        Ok(Arc::new(Self {
            state: WorkerState::new(fp, info, skip_flush),
            worker_count,
            workers: ThreadGroup::new(),
        }))
    }
    fn spawn_workers(&self) -> std::io::Result<()> {
        let spawn_results = (0..self.worker_count.get())
            .map(|n| {
                let worker_state = self.state.clone();
                let worker_acc = self
                    .state
                    .attachment
                    .accessor_mem(|mem| mem.child(Some(format!("worker {n}"))))
                    .expect("backend is attached");

                std::thread::Builder::new()
                    .name(format!("file worker {n}"))
                    .spawn(move || {
                        worker_state.processing_loop(worker_acc);
                    })
            })
            .collect::<Vec<_>>();

        self.workers.extend(spawn_results.into_iter())
    }
}

impl block::Backend for FileBackend {
    fn attachment(&self) -> &block::BackendAttachment {
        &self.state.attachment
    }

    fn info(&self) -> DeviceInfo {
        self.state.info
    }
    fn start(&self) -> anyhow::Result<()> {
        self.state.attachment.start();
        if let Err(e) = self.spawn_workers() {
            self.state.attachment.stop();
            self.workers.block_until_joined();
            Err(e).context("failure while spawning workers")
        } else {
            Ok(())
        }
    }
    fn stop(&self) {
        self.state.attachment.stop();
        self.workers.block_until_joined();
    }
}

/// Attempt to query the Write-Cache-Enable state for a given open device
///
/// Block devices (including "real" disks and zvols) will regard all writes as
/// synchronous when performed via the /dev/rdsk endpoint when WCE is not
/// enabled.  With WCE enabled, writes can be cached on the device, to be
/// flushed later via fsync().
fn get_wce(fp: &File) -> Option<bool> {
    let ft = fp.metadata().ok()?.file_type();
    if !ft.is_char_device() {
        return None;
    }

    let mut res: c_int = 0;
    let _ = unsafe {
        ioctl(fp.as_raw_fd(), DKIOCGETWCE, &mut res as *mut c_int as _).ok()?
    };
    Some(res != 0)
}

/// Attempt to set the Write-Cache-Enable state for a given open device
fn set_wce(fp: &File, enabled: bool) -> Result<bool> {
    let mut flag: c_int = enabled.into();
    unsafe {
        ioctl(fp.as_raw_fd(), DKIOCSETWCE, &mut flag as *mut c_int as _)
            .map(|_| enabled)
    }
}
