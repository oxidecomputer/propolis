//! Implements an interface to virtualized block devices.

use std::collections::VecDeque;
use std::fs::{metadata, File, OpenOptions};
use std::io::Result;
use std::io::{Error, ErrorKind};
use std::os::unix::io::AsRawFd;
use std::path::Path;
use std::sync::Condvar;
use std::sync::{Arc, Mutex, Weak};

use crate::common::*;
use crate::dispatch::{DispCtx, Dispatcher};
use crate::vmm::{MemCtx, SubMapping};

/// Type of operations which may be issued to a virtual block device.
#[derive(Copy, Clone, Debug)]
pub enum BlockOp {
    Flush,
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

/// Abstraction over an actual backing store
pub trait BlockDevBackingStore: Send + Sync + 'static {
    fn issue_read(
        &self,
        offset: usize,
        sz: usize,
        mapping: SubMapping,
    ) -> Result<usize>;
    fn issue_write(
        &self,
        offset: usize,
        sz: usize,
        mapping: SubMapping,
    ) -> Result<usize>;
    fn issue_flush(&self) -> Result<()>;

    fn is_ro(&self) -> bool;

    // length in bytes
    fn len(&self) -> usize;
}

/// A block device implementation backed by an underlying file.
pub struct FileBlockDevBackingStore {
    fp: File,
    is_ro: bool,
    len: usize,
}

impl FileBlockDevBackingStore {
    pub fn from_path(
        path: impl AsRef<Path>,
        readonly: bool,
    ) -> Result<FileBlockDevBackingStore> {
        let p: &Path = path.as_ref();

        let meta = metadata(p)?;
        let is_ro = readonly || meta.permissions().readonly();

        let fp = OpenOptions::new().read(true).write(!is_ro).open(p)?;
        let len = fp.metadata().unwrap().len() as usize;

        Ok(FileBlockDevBackingStore { fp, is_ro, len })
    }

    pub fn get_file(&self) -> &File {
        &self.fp
    }
}

impl BlockDevBackingStore for FileBlockDevBackingStore {
    fn issue_read(
        &self,
        offset: usize,
        sz: usize,
        mapping: SubMapping,
    ) -> Result<usize> {
        mapping.pread(&self.fp, sz, offset as i64)
    }

    fn issue_write(
        &self,
        offset: usize,
        sz: usize,
        mapping: SubMapping,
    ) -> Result<usize> {
        mapping.pwrite(&self.fp, sz, offset as i64)
    }

    fn issue_flush(&self) -> Result<()> {
        let res = unsafe { libc::fdatasync(self.fp.as_raw_fd()) };

        if res == -1 {
            Err(Error::new(ErrorKind::Other, "file flush failed"))
        } else {
            Ok(())
        }
    }

    fn is_ro(&self) -> bool {
        self.is_ro
    }

    fn len(&self) -> usize {
        self.len
    }
}

/// Standard [`BlockDev`] implementation. Requires a backing store.
pub struct PlainBdev<R: BlockReq, S: BlockDevBackingStore> {
    backing_store: S,
    block_size: usize,
    sectors: usize,
    reqs: Mutex<VecDeque<R>>,
    cond: Condvar,
}

impl<R: BlockReq, S: BlockDevBackingStore> PlainBdev<R, S> {
    /// Creates a new block device from a device at `path`.
    pub fn create(backing_store: S) -> Result<Arc<Self>> {
        let len = backing_store.len();

        let this = Self {
            backing_store,
            block_size: 512,
            sectors: len / 512,
            reqs: Mutex::new(VecDeque::new()),
            cond: Condvar::new(),
        };

        Ok(Arc::new(this))
    }

    // Consume enqueued requests and process them. Signal completion when done.
    fn process_loop(&self, ctx: &mut DispCtx) {
        let mut reqs = self.reqs.lock().unwrap();
        loop {
            if ctx.check_yield() {
                break;
            }

            if let Some(mut req) = reqs.pop_front() {
                let result = self.process_request(&mut req, ctx);
                req.complete(result, ctx);
            } else {
                reqs = self.cond.wait(reqs).unwrap();
            }
        }
    }

    // Match against individual BlockReqs and call appropriate processing functions.
    fn process_request(&self, req: &mut R, ctx: &DispCtx) -> BlockResult {
        let mem = ctx.mctx.memctx();

        let mut offset = req.offset();

        // TODO: are buffers guaranteed to be sector size? do BlockDevBackingStores require
        // this?
        while let Some(buf) = req.next_buf() {
            let sz = buf.1;

            let result = match req.oper() {
                BlockOp::Read => {
                    self.process_read_request(offset, sz, &mem, buf)
                }
                BlockOp::Write => self.process_write_request(offset, &mem, buf),
                BlockOp::Flush => self.process_flush(),
            };

            // If any BlockReq in this IO request fail, inform the guest that
            // their IO request failed.
            match result {
                BlockResult::Success => {} // ok
                BlockResult::Failure => {
                    return BlockResult::Failure;
                }
                BlockResult::Unsupported => {
                    return BlockResult::Unsupported;
                }
            };

            offset += sz;
        }

        BlockResult::Success
    }

    // Read from BlockReqBackingStore and write into VM memory.
    fn process_read_request(
        &self,
        offset: usize,
        sz: usize,
        mem: &MemCtx,
        buf: GuestRegion,
    ) -> BlockResult {
        if let Some(mapping) = mem.writable_region(&buf) {
            let nread = self.backing_store.issue_read(offset, sz, mapping);
            match nread {
                Ok(nread) => {
                    assert_eq!(nread as usize, sz);
                    BlockResult::Success
                }
                Err(_) => {
                    // TODO: Error writing to guest memory
                    BlockResult::Failure
                }
            }
        } else {
            // TODO: Error getting writable region
            BlockResult::Failure
        }
    }

    // Read from VM memory and write to BlockReqBackingStore.
    fn process_write_request(
        &self,
        offset: usize,
        mem: &MemCtx,
        buf: GuestRegion,
    ) -> BlockResult {
        let sz = buf.1;

        if let Some(mapping) = mem.readable_region(&buf) {
            let nwritten = self.backing_store.issue_write(offset, sz, mapping);
            match nwritten {
                Ok(nwritten) => {
                    assert_eq!(nwritten as usize, sz);
                    BlockResult::Success
                }
                Err(_) => {
                    // TODO: Error writing to guest memory
                    BlockResult::Failure
                }
            }
        } else {
            // TODO: Error getting readable region
            BlockResult::Failure
        }
    }

    // Send flush to BlockReqBackingStore
    fn process_flush(&self) -> BlockResult {
        match self.backing_store.issue_flush() {
            Ok(()) => BlockResult::Success,
            Err(_) => {
                // TODO: encapsulated error here from BlockReqBackingStore here?
                BlockResult::Failure
            }
        }
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

impl<R: BlockReq, S: BlockDevBackingStore> BlockDev<R> for PlainBdev<R, S> {
    fn enqueue(&self, req: R) {
        self.reqs.lock().unwrap().push_back(req);
        self.cond.notify_all();
    }

    fn inquire(&self) -> BlockInquiry {
        BlockInquiry {
            total_size: self.sectors as u64,
            block_size: self.block_size as u32,
            writable: !self.backing_store.is_ro(),
        }
    }
}

pub fn create_file_backed_block_device<R: BlockReq>(
    path: impl AsRef<Path>,
    readonly: bool,
) -> Result<Arc<PlainBdev<R, FileBlockDevBackingStore>>> {
    let backing_store = FileBlockDevBackingStore::from_path(path, readonly)?;
    let plain_bdev =
        PlainBdev::<R, FileBlockDevBackingStore>::create(backing_store)?;

    Ok(plain_bdev)
}
