// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::fs::{metadata, File, OpenOptions};
use std::io::{Error, ErrorKind, Result};
use std::num::NonZeroUsize;
use std::os::unix::io::AsRawFd;
use std::path::Path;
use std::sync::{Arc, Weak};

use super::DeviceInfo;
use crate::block;
use crate::inventory::Entity;
use crate::vmm::{MappingExt, MemCtx};

// XXX: completely arb for now
const MAX_WORKERS: usize = 32;

/// Standard [`Backend`](super::Backend) implementation.
pub struct FileBackend {
    fp: Arc<File>,
    driver: block::Driver,
    log: slog::Logger,

    read_only: bool,
    block_size: usize,
    sectors: usize,
}

impl FileBackend {
    /// Creates a new block device from a device at `path`.
    pub fn create(
        path: impl AsRef<Path>,
        readonly: bool,
        worker_count: NonZeroUsize,
        log: slog::Logger,
    ) -> Result<Arc<Self>> {
        if worker_count.get() > MAX_WORKERS {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "too many workers",
            ));
        }
        let p: &Path = path.as_ref();

        let meta = metadata(p)?;
        let read_only = readonly || meta.permissions().readonly();

        let fp = OpenOptions::new().read(true).write(!read_only).open(p)?;
        let len = fp.metadata().unwrap().len() as usize;

        Ok(Arc::new_cyclic(|me| Self {
            fp: Arc::new(fp),
            driver: block::Driver::new(
                me.clone() as Weak<dyn block::Backend>,
                "file-bdev".to_string(),
                worker_count,
            ),
            log,

            read_only,
            block_size: 512,
            sectors: len / 512,
        }))
    }
    fn process_request(
        &self,
        req: &block::Request,
        mem: &MemCtx,
    ) -> Result<()> {
        match req.oper() {
            block::Operation::Read(off) => {
                let maps = req.mappings(mem).ok_or_else(|| {
                    Error::new(ErrorKind::Other, "bad guest region")
                })?;

                let nbytes = maps.preadv(self.fp.as_raw_fd(), off as i64)?;
                if nbytes != req.len() {
                    return Err(Error::new(
                        ErrorKind::Other,
                        "bad read length",
                    ));
                }
            }
            block::Operation::Write(off) => {
                if self.read_only {
                    return Err(Error::new(
                        ErrorKind::PermissionDenied,
                        "backend is read-only",
                    ));
                }

                let maps = req.mappings(mem).ok_or_else(|| {
                    Error::new(ErrorKind::Other, "bad guest region")
                })?;

                let nbytes = maps.pwritev(self.fp.as_raw_fd(), off as i64)?;
                if nbytes != req.len() {
                    return Err(Error::new(
                        ErrorKind::Other,
                        "bad write length",
                    ));
                }
            }
            block::Operation::Flush(_off, _len) => {
                self.fp.sync_data()?;
            }
        }
        Ok(())
    }
}

impl block::Backend for FileBackend {
    fn info(&self) -> DeviceInfo {
        DeviceInfo {
            block_size: self.block_size as u32,
            total_size: self.sectors as u64,
            writable: !self.read_only,
        }
    }

    fn attach(&self, dev: Arc<dyn block::Device>) -> Result<()> {
        self.driver.attach(dev)
    }

    fn process(&self, req: &block::Request, mem: &MemCtx) -> block::Result {
        match self.process_request(req, mem) {
            Ok(_) => block::Result::Success,
            Err(e) => {
                slog::info!(self.log, "block IO error {:?}", req.op; "error" => e);
                block::Result::Failure
            }
        }
    }
}
impl Entity for FileBackend {
    fn type_name(&self) -> &'static str {
        "block-file"
    }
    fn start(&self) -> anyhow::Result<()> {
        self.driver.start();
        Ok(())
    }
    fn pause(&self) {
        self.driver.pause();
    }
    fn resume(&self) {
        self.driver.resume();
    }
    fn halt(&self) {
        self.driver.halt();
    }
}
