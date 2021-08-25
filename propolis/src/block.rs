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
#[derive(Copy, Clone, Debug, PartialEq)]
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
pub trait BackingStore: Send + Sync + 'static {
    /// Depending on the is_read switch:
    /// - perform a read request: read from backing store and write into guest mappings
    /// - perform a write request: read from guest mappings and write into backing store
    ///
    /// The entire request to the backing store starts at some offset, and the total
    /// size is the sum of all the mapping's size:
    ///
    /// |-----m1-----|--m2--|-m3-|----------m4----------|
    /// ^                                               ^
    /// offset                                          offset plus total size
    ///
    /// Don't assume mappings are contiguous in guest memory, or that they are ordered.
    /// Guests can choose to send whatever they want.
    ///
    /// Backing stores can choose to perform the above example as one large request, or
    /// four small ones.
    fn issue_rw_request(
        &self,
        is_read: bool,
        offset: usize,
        mappings: Vec<SubMapping>,
    ) -> Result<usize>;

    /// Issue flush command to backing store
    fn issue_flush(&self) -> Result<()>;

    /// Is backing store read only?
    fn is_ro(&self) -> bool;

    /// return total length in bytes
    fn len(&self) -> usize;

    /// some backing stores may have block size requirements. optionally return a
    /// block size in bytes.
    fn block_size(&self) -> Option<usize>;
}

/// A block device implementation backed by an underlying file.
pub struct FileBackingStore {
    fp: File,
    is_ro: bool,
    len: usize,
}

impl FileBackingStore {
    pub fn from_path(
        path: impl AsRef<Path>,
        readonly: bool,
    ) -> Result<FileBackingStore> {
        let p: &Path = path.as_ref();

        let meta = metadata(p)?;
        let is_ro = readonly || meta.permissions().readonly();

        let fp = OpenOptions::new().read(true).write(!is_ro).open(p)?;
        let len = fp.metadata().unwrap().len() as usize;

        Ok(FileBackingStore { fp, is_ro, len })
    }

    pub fn get_file(&self) -> &File {
        &self.fp
    }
}

impl BackingStore for FileBackingStore {
    fn issue_rw_request(
        &self,
        is_read: bool,
        offset: usize,
        mappings: Vec<SubMapping>,
    ) -> Result<usize> {
        let mut nbytes = 0;

        for mapping in mappings {
            let inner_nbytes = if is_read {
                mapping.pread(
                    &self.fp,
                    mapping.len(),
                    (offset + nbytes) as i64,
                )?
            } else {
                mapping.pwrite(
                    &self.fp,
                    mapping.len(),
                    (offset + nbytes) as i64,
                )?
            };

            if inner_nbytes != mapping.len() {
                return Err(Error::new(
                    ErrorKind::Other,
                    format!(
                        "{} at offset {} of size {} incomplete! only {} bytes",
                        if is_read { "read" } else { "write" },
                        offset + nbytes,
                        mapping.len(),
                        inner_nbytes,
                    ),
                ));
            }

            nbytes += inner_nbytes;
        }

        Ok(nbytes)
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

    fn block_size(&self) -> Option<usize> {
        // Files do not have a block size requirement
        None
    }
}

/// Standard [`BlockDev`] implementation. Requires a backing store.
pub struct PlainBdev<R: BlockReq, S: BackingStore> {
    backing_store: S,
    block_size: usize,
    sectors: usize,
    reqs: Mutex<VecDeque<R>>,
    cond: Condvar,
}

impl<R: BlockReq> PlainBdev<R, FileBackingStore> {
    pub fn from_file(
        path: impl AsRef<Path>,
        readonly: bool,
    ) -> Result<Arc<PlainBdev<R, FileBackingStore>>> {
        let backing_store = FileBackingStore::from_path(path, readonly)?;
        let plain_bdev = PlainBdev::create(backing_store)?;

        Ok(plain_bdev)
    }
}

impl<R: BlockReq, S: BackingStore> PlainBdev<R, S> {
    /// Creates a new block device from a device at `path`.
    pub fn create(backing_store: S) -> Result<Arc<Self>> {
        let len = backing_store.len();
        let block_size = backing_store.block_size().unwrap_or(512);

        let this = Self {
            backing_store,
            block_size,
            sectors: len / block_size,
            reqs: Mutex::new(VecDeque::new()),
            cond: Condvar::new(),
        };

        Ok(Arc::new(this))
    }

    /// Consume enqueued requests and process them. Signal completion when done.
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

    /// Gather all buffers from the request and pass as a group to the appropriate backing store
    /// processing function.
    fn process_request(&self, req: &mut R, ctx: &DispCtx) -> BlockResult {
        let mem = ctx.mctx.memctx();

        let offset = req.offset();

        let mut bufs = vec![];

        while let Some(buf) = req.next_buf() {
            bufs.push(buf);
        }

        let result = match req.oper() {
            BlockOp::Read => self.process_rw_request(true, offset, &mem, bufs),
            BlockOp::Write => {
                self.process_rw_request(false, offset, &mem, bufs)
            }
            BlockOp::Flush => self.process_flush(),
        };

        match result {
            Ok(status) => status,
            Err(_) => BlockResult::Failure,
        }
    }

    /// Delegate a block device read or write to the backing store.
    fn process_rw_request(
        &self,
        is_read: bool,
        offset: usize,
        mem: &MemCtx,
        bufs: Vec<GuestRegion>,
    ) -> Result<BlockResult> {
        let mappings: Vec<SubMapping> = bufs
            .iter()
            .map(|buf| {
                if is_read {
                    mem.writable_region(buf).unwrap()
                } else {
                    mem.readable_region(buf).unwrap()
                }
            })
            .collect();

        let total_size: usize = mappings.iter().map(|x| x.len()).sum();

        let nbytes =
            self.backing_store.issue_rw_request(is_read, offset, mappings);
        match nbytes {
            Ok(nbytes) => {
                assert_eq!(nbytes as usize, total_size);
                Ok(BlockResult::Success)
            }
            Err(_) => {
                // TODO: Error writing to guest memory
                Ok(BlockResult::Failure)
            }
        }
    }

    /// Send flush to the backing store.
    fn process_flush(&self) -> Result<BlockResult> {
        match self.backing_store.issue_flush() {
            Ok(_) => Ok(BlockResult::Success),
            Err(_) => Ok(BlockResult::Failure),
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

impl<R: BlockReq, S: BackingStore> BlockDev<R> for PlainBdev<R, S> {
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

#[cfg(test)]
mod test {
    use std::fs::File;
    use std::io::{Read, Seek, SeekFrom, Write};

    use tempfile::tempdir;

    use crate::block::{BackingStore, FileBackingStore};
    use crate::vmm::mapping::{GuardSpace, Prot};

    #[test]
    fn test_file_backing_store() {
        /*
         * Test the following:
         * - seed a 512 byte file with a known pattern
         * - read that file into four mappings
         * - verify that the known pattern was read correctly into those four mappings
         * - write another known pattern into another mapping
         * - write from that mapping into the file
         * - verify that the file received the full write
         */
        let dir = tempdir().expect("cannot create tempdir!");
        let file_path = dir.path().join("disk.img");

        let mut file =
            File::create(file_path.clone()).expect("cannot create tempfile!");
        file.set_len(512).unwrap();

        let backing_store =
            FileBackingStore::from_path(file_path.clone(), false)
                .expect("could not create FileBackingStore!");
        assert_eq!(512, backing_store.len());

        let guard_len = 4096;
        let mut guard = GuardSpace::new(guard_len).unwrap();
        let vmm = crate::vmm::mapping::tests::test_vmm(guard_len as u64);
        let mapping = guard
            .mapping(guard_len, Prot::READ | Prot::WRITE, &vmm, 0)
            .unwrap();

        // write into file
        file.seek(SeekFrom::Start(0)).expect("seek failed!");
        file.write(&vec![0; 128][..]).expect("write failed!");
        file.write(&vec![1; 128][..]).expect("write failed!");
        file.write(&vec![2; 128][..]).expect("write failed!");
        file.write(&vec![3; 128][..]).expect("write failed!");

        // read into mappings
        let mappings = vec![
            mapping.as_ref().subregion(25, 7).unwrap(),
            mapping.as_ref().subregion(75, 256).unwrap(),
            mapping.as_ref().subregion(1350, 128).unwrap(),
            mapping.as_ref().subregion(2048, 121).unwrap(),
        ];
        backing_store
            .issue_rw_request(true, 0, mappings)
            .expect("read request failed!");
        backing_store.issue_flush().expect("flush failed!");

        // verify mapping[0] only got zeros
        let mut bytes = vec![100; 7];
        mapping
            .as_ref()
            .subregion(25, 7)
            .unwrap()
            .read_bytes(&mut bytes[..])
            .unwrap();
        assert_eq!(&bytes, &vec![0u8; 7]);

        // verify mapping[1] got zeros, ones, and twos
        // 000000011111111111111111111111122222222
        // ^      ^                       ^      ^
        // 7      128                     256    263
        //
        let mut bytes = vec![100; 256];
        mapping
            .as_ref()
            .subregion(75, 256)
            .unwrap()
            .read_bytes(&mut bytes[..])
            .unwrap();

        let mut expected = vec![0u8; 128 - 7];
        expected.append(&mut vec![1u8; 128]);
        expected.append(&mut vec![2u8; 7]);
        assert_eq!(bytes, expected);

        // verify mapping[2] got twos and threes
        // 222222222222223333333
        // ^             ^     ^
        // 263           384   391
        let mut bytes = vec![100; 128];
        mapping
            .as_ref()
            .subregion(1350, 128)
            .unwrap()
            .read_bytes(&mut bytes[..])
            .unwrap();

        let mut expected = vec![2u8; 384 - 263];
        expected.append(&mut vec![3u8; 391 - 384]);
        assert_eq!(bytes, expected);

        // verify mapping[3] got threes
        let mut bytes = vec![100; 121];
        mapping
            .as_ref()
            .subregion(2048, 121)
            .unwrap()
            .read_bytes(&mut bytes[..])
            .unwrap();

        let expected = vec![3u8; 121];
        assert_eq!(bytes, expected);

        // write into file

        let mut bytes = vec![100u8; 512];
        mapping
            .as_ref()
            .subregion(3072, 512)
            .unwrap()
            .write_bytes(&mut bytes[..])
            .unwrap();

        let mappings = vec![mapping.as_ref().subregion(3072, 512).unwrap()];

        backing_store
            .issue_rw_request(false, 0, mappings)
            .expect("write request failed!");
        backing_store.issue_flush().expect("flush failed!");

        // verify write to file

        let mut file = File::open(file_path).expect("cannot open tempfile!");

        let mut buffer = vec![0; 512];
        file.seek(SeekFrom::Start(0)).expect("seek failed!");
        file.read(&mut buffer[..]).expect("buffer read failed!");
        assert_eq!(buffer, vec![100u8; 512]);

        drop(file);
        dir.close().expect("could not close dir!");
    }
}
