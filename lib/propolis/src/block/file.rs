// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::fs::{metadata, File, OpenOptions};
use std::io::{Error, ErrorKind, Result};
use std::num::NonZeroUsize;
use std::os::unix::io::AsRawFd;
use std::path::Path;
use std::sync::Arc;

use crate::accessors::MemAccessor;
use crate::block::{self, DeviceInfo};
use crate::vmm::{MappingExt, MemCtx};

// XXX: completely arb for now
const MAX_WORKERS: usize = 32;

pub struct FileBackend {
    state: Arc<WorkerState>,

    worker_count: NonZeroUsize,
}
struct WorkerState {
    attachment: block::backend::Attachment,
    fp: File,

    info: block::DeviceInfo,
    skip_flush: bool,
}
impl WorkerState {
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

        Ok(Arc::new(Self {
            state: Arc::new(WorkerState {
                attachment: block::backend::Attachment::new(),

                fp,

                skip_flush: opts.skip_flush.unwrap_or(false),
                info: block::DeviceInfo {
                    block_size,
                    total_size: len / block_size as u64,
                    read_only,
                },
            }),
            worker_count,
        }))
    }
    fn spawn_workers(&self) -> std::io::Result<()> {
        for n in 0..self.worker_count.get() {
            let worker_state = self.state.clone();
            let worker_acc = self.state.attachment.accessor_mem(|mem| {
                mem.expect("backend is attached")
                    .child(Some(format!("worker {n}")))
            });

            let _join = std::thread::Builder::new()
                .name(format!("file worker {n}"))
                .spawn(move || {
                    worker_state.processing_loop(worker_acc);
                })?;
        }
        Ok(())
    }
}

impl block::Backend for FileBackend {
    fn attachment(&self) -> &block::backend::Attachment {
        &self.state.attachment
    }

    fn info(&self) -> DeviceInfo {
        self.state.info
    }
    fn start(&self) -> anyhow::Result<()> {
        self.spawn_workers()?;
        self.state.attachment.start();
        Ok(())
    }
}
