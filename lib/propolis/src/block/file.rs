use std::fs::{metadata, File, OpenOptions};
use std::io::{Error, ErrorKind, Result};
use std::num::NonZeroUsize;
use std::os::unix::io::AsRawFd;
use std::path::Path;
use std::sync::{Arc, Mutex};

use super::DeviceInfo;
use crate::block;
use crate::dispatch::{DispCtx, Dispatcher};
use crate::inventory::Entity;
use crate::vmm::MappingExt;

// XXX: completely arb for now
const MAX_WORKERS: usize = 32;

/// Standard [`BlockDev`] implementation.
pub struct FileBackend {
    fp: Arc<File>,

    driver: Mutex<Option<Arc<block::Driver>>>,
    worker_count: NonZeroUsize,

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

        let this = Self {
            fp: Arc::new(fp),

            driver: Mutex::new(None),
            worker_count,

            read_only,
            block_size: 512,
            sectors: len / 512,
        };

        Ok(Arc::new(this))
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

    fn attach(
        &self,
        dev: Arc<dyn block::Device>,
        disp: &Dispatcher,
    ) -> Result<()> {
        let mut driverg = self.driver.lock().unwrap();
        assert!(driverg.is_none());

        let fp = Arc::clone(&self.fp);
        let read_only = self.read_only;
        let req_handler =
            Box::new(move |req: &block::Request, ctx: &DispCtx| {
                process_request(&fp, req, ctx, read_only)
            });

        // Spawn driver to service block dev requests
        let driver = Arc::new(block::Driver::new(dev, req_handler));
        driver.spawn("file", self.worker_count, disp)?;
        *driverg = Some(driver);

        Ok(())
    }
}
impl Entity for FileBackend {
    fn type_name(&self) -> &'static str {
        "block-file"
    }
}

fn process_request(
    fp: &File,
    req: &block::Request,
    ctx: &DispCtx,
    read_only: bool,
) -> Result<()> {
    let mem = ctx.mctx.memctx();
    match req.oper() {
        block::Operation::Read(off) => {
            let maps = req.mappings(&mem).ok_or_else(|| {
                Error::new(ErrorKind::Other, "bad guest region")
            })?;

            let nbytes = maps.preadv(fp.as_raw_fd(), off as i64)?;
            if nbytes != req.len() {
                return Err(Error::new(ErrorKind::Other, "bad read length"));
            }
        }
        block::Operation::Write(off) => {
            if read_only {
                return Err(Error::new(
                    ErrorKind::PermissionDenied,
                    "backend is read-only",
                ));
            }

            let maps = req.mappings(&mem).ok_or_else(|| {
                Error::new(ErrorKind::Other, "bad guest region")
            })?;

            let nbytes = maps.pwritev(fp.as_raw_fd(), off as i64)?;
            if nbytes != req.len() {
                return Err(Error::new(ErrorKind::Other, "bad write length"));
            }
        }
        block::Operation::Flush(_off, _len) => {
            fp.sync_data()?;
        }
    }
    Ok(())
}
