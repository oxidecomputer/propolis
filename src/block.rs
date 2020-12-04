use std::collections::VecDeque;
use std::fs::{metadata, File, OpenOptions};
use std::io::Result;
use std::os::unix::fs::FileTypeExt;
use std::os::unix::io::{AsRawFd, RawFd};
use std::path::Path;
use std::sync::Condvar;
use std::sync::{Arc, Mutex};

use crate::common::*;
use crate::dispatch::{DispCtx, Dispatcher};

use libc::{c_void, pread, pwrite};

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

pub trait BlockReq: Send + Sync + 'static {
    fn oper(&self) -> BlockOp;
    fn offset(&self) -> usize;
    fn next_buf(&mut self) -> Option<GuestRegion>;
    fn complete(self, res: BlockResult, ctx: &DispCtx);
}

#[derive(Debug)]
pub struct BlockInquiry {
    /// Device size in blocks (see below)
    pub total_size: u64,
    /// Size (in bytes) per block
    pub block_size: u32,
    pub writable: bool,
}

pub trait BlockDev<R: BlockReq>: Send + Sync + 'static {
    fn enqueue(&self, req: R);
    fn inquire(&self) -> BlockInquiry;
}

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
    pub fn new(path: impl AsRef<Path>) -> Result<Arc<Self>> {
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
    fn process_loop(&self, ctx: &DispCtx) {
        let mut reqs = self.reqs.lock().unwrap();
        loop {
            reqs = self.cond.wait_while(reqs, |r| r.is_empty()).unwrap();
            while let Some(mut req) = reqs.pop_front() {
                let res = match req.oper() {
                    BlockOp::Read => self.process_read(&mut req, ctx),
                    BlockOp::Write => self.process_write(&mut req, ctx),
                };
                req.complete(res, ctx);
            }
        }
    }
    fn process_read(&self, req: &mut R, ctx: &DispCtx) -> BlockResult {
        let mem = ctx.mctx.memctx();

        let mut offset = req.offset();
        while let Some(buf) = req.next_buf() {
            if let Some(rbuf) = mem.raw_writable(&buf) {
                let nread = unsafe {
                    pread(self.fd, rbuf as *mut c_void, buf.1, offset as i64)
                };
                if nread == -1 {
                    // XXX: error reporting
                    return BlockResult::Failure;
                }
                assert_eq!(nread as usize, buf.1);
                offset += buf.1;
            } else {
                // XXX: report bad addr
                return BlockResult::Failure;
            }
        }
        BlockResult::Success
    }
    fn process_write(&self, req: &mut R, ctx: &DispCtx) -> BlockResult {
        let mem = ctx.mctx.memctx();

        let mut offset = req.offset();
        while let Some(buf) = req.next_buf() {
            if let Some(wbuf) = mem.raw_readable(&buf) {
                let nwritten = unsafe {
                    pwrite(self.fd, wbuf as *const c_void, buf.1, offset as i64)
                };
                if nwritten == -1 {
                    // XXX: error reporting
                    return BlockResult::Failure;
                }
                assert_eq!(nwritten as usize, buf.1);
                offset += buf.1;
            } else {
                // XXX: report bad addr
                return BlockResult::Failure;
            }
        }
        BlockResult::Success
    }
    pub fn start_dispatch(self: Arc<Self>, name: String, disp: &Dispatcher) {
        disp.spawn(name, self, |dctx, bdev| {
            bdev.process_loop(&dctx);
        })
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
