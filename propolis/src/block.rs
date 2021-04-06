//! Implements an interface to virtualized block devices.

use std::collections::VecDeque;
use std::fs::{metadata, File, OpenOptions};
use std::io::Result;
use std::os::unix::fs::FileTypeExt;
use std::os::unix::io::{AsRawFd, RawFd};
use std::path::Path;
use std::sync::Condvar;
use std::sync::{Arc, Mutex, Weak};

use crate::common::*;
use crate::dispatch::{DispCtx, Dispatcher};

/// Type of operations which may be issued to a virtual block device.
#[derive(Copy, Clone, Debug)]
pub enum BlockOp {
    Read,
    Write,
}

#[derive(Copy, Clone, Debug)]
pub enum BlockResult {
    Success,
    Failure,
    Unsupported,
}

/// Trait indicating that a type may be used as a request to a block device.
pub trait BlockReq: Send + Sync + 'static {
    /// Type of operation being issued.
    fn oper(&self) -> BlockOp;
    /// Offset within the block device, in bytes.
    fn offset(&self) -> usize;
    /// Returns the next region of memory within a request to a block device.
    fn next_buf(&mut self) -> Option<GuestRegion>;
    /// Signals to the device emulation that a block operation has been completed.
    fn complete(self, res: BlockResult, ctx: &DispCtx);
}

/// Metadata regarding a virtualized block device.
#[derive(Debug)]
pub struct BlockInquiry {
    /// Device size in blocks (see below)
    pub total_size: u64,
    /// Size (in bytes) per block
    pub block_size: u32,
    pub writable: bool,
}

/// API to access a virtualized block device.
pub trait BlockDev<R: BlockReq>: Send + Sync + 'static {
    /// Enqueues a [`BlockReq`] to the underlying device.
    fn enqueue(&self, req: R);
    /// Requests metadata about the block device.
    fn inquire(&self) -> BlockInquiry;
}

/// A block device implementation backed by an underlying file.
///
/// Primarily accessed through the [`BlockDev`] interface.
pub struct PlainBdev<R: BlockReq> {
    fp: File,
    fd: RawFd,
    is_ro: bool,
    is_raw: bool,
    block_size: usize,
    sectors: usize,
    reqs: Mutex<VecDeque<R>>,
    cond: Condvar,
}
impl<R: BlockReq> PlainBdev<R> {
    /// Creates a new block device from a device at `path`.
    pub fn create(path: impl AsRef<Path>) -> Result<Arc<Self>> {
        let p: &Path = path.as_ref();

        let meta = metadata(p)?;
        let is_ro = meta.permissions().readonly();

        let fp = OpenOptions::new().read(true).write(!is_ro).open(p)?;
        let is_raw = fp.metadata()?.file_type().is_char_device();
        let fd = fp.as_raw_fd();

        let mut this = Self {
            fp,
            fd,
            is_ro,
            block_size: 512,
            sectors: 0,
            is_raw,
            reqs: Mutex::new(VecDeque::new()),
            cond: Condvar::new(),
        };
        this.raw_init();

        Ok(Arc::new(this))
    }
    fn raw_init(&mut self) {
        // TODO: query block size, write cache, discard, etc
        assert!(!self.is_raw);
        let len = self.fp.metadata().unwrap().len() as usize;
        self.sectors = len / self.block_size;
    }
    fn process_loop(&self, ctx: &mut DispCtx) {
        let mut reqs = self.reqs.lock().unwrap();
        loop {
            if ctx.check_yield() {
                break;
            }

            if let Some(mut req) = reqs.pop_front() {
                let res = match req.oper() {
                    BlockOp::Read => self.process_read(&mut req, ctx),
                    BlockOp::Write => self.process_write(&mut req, ctx),
                };
                req.complete(res, ctx);
            } else {
                reqs = self.cond.wait(reqs).unwrap();
            }
        }
    }
    fn process_read(&self, req: &mut R, ctx: &DispCtx) -> BlockResult {
        let mem = ctx.mctx.memctx();

        let mut offset = req.offset();
        while let Some(buf) = req.next_buf() {
            if let Some(mapping) = mem.writable_region(&buf) {
                if let Ok(nread) = mapping.pread(self.fd, buf.1, offset as i64)
                {
                    assert_eq!(nread as usize, buf.1);
                    offset += buf.1;
                } else {
                    // XXX: Error reporting (bad read)
                    return BlockResult::Failure;
                }
            } else {
                // XXX: Error reporting (bad addr)
                return BlockResult::Failure;
            }
        }
        BlockResult::Success
    }
    fn process_write(&self, req: &mut R, ctx: &DispCtx) -> BlockResult {
        let mem = ctx.mctx.memctx();

        let mut offset = req.offset();
        while let Some(buf) = req.next_buf() {
            if let Some(mapping) = mem.readable_region(&buf) {
                if let Ok(nwritten) =
                    mapping.pwrite(self.fd, buf.1, offset as i64)
                {
                    assert_eq!(nwritten as usize, buf.1);
                    offset += buf.1;
                } else {
                    // XXX: Error reporting (bad write)
                    return BlockResult::Failure;
                }
            } else {
                // XXX: Error reporting (bad addr)
                return BlockResult::Failure;
            }
        }
        BlockResult::Success
    }

    /// Spawns a new thread named `name` on the dispatcher `disp` which
    /// begins processing incoming requests.
    pub fn start_dispatch(self: Arc<Self>, name: String, disp: &Dispatcher) {
        let ww = Arc::downgrade(&self);

        disp.spawn(
            name,
            self,
            |bdev, ctx| {
                bdev.process_loop(ctx);
            },
            Some(Box::new(move |_ctx| {
                if let Some(this) = Weak::upgrade(&ww) {
                    this.cond.notify_all()
                }
            })),
        )
        .unwrap();
    }
}

impl<R: BlockReq> BlockDev<R> for PlainBdev<R> {
    fn enqueue(&self, req: R) {
        self.reqs.lock().unwrap().push_back(req);
        self.cond.notify_all();
    }

    fn inquire(&self) -> BlockInquiry {
        BlockInquiry {
            total_size: self.sectors as u64,
            block_size: self.block_size as u32,
            writable: !self.is_ro,
        }
    }
}
