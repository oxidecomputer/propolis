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

/// Standard [`BlockDev`] implementation.
pub struct PlainBdev<R: BlockReq> {
    fp: File,
    is_ro: bool,

    block_size: usize,
    sectors: usize,
    reqs: Mutex<VecDeque<R>>,
    cond: Condvar,
}

impl<R: BlockReq> PlainBdev<R> {
    /// Creates a new block device from a device at `path`.
    pub fn create(path: impl AsRef<Path>, readonly: bool) -> Result<Arc<Self>> {
        let p: &Path = path.as_ref();

        let meta = metadata(p)?;
        let is_ro = readonly || meta.permissions().readonly();

        let fp = OpenOptions::new().read(true).write(!is_ro).open(p)?;
        let len = fp.metadata().unwrap().len() as usize;

        let this = Self {
            fp: fp,
            is_ro,

            block_size: 512,
            sectors: len / 512,
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

    /// Gather all buffers from the request and pass as a group to the appropriate processing function.
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

    /// Delegate a block device read or write to the file.
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

        let nbytes = {
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
                    println!(
                        "{} at offset {} of size {} incomplete! only {} bytes",
                        if is_read { "read" } else { "write" },
                        offset + nbytes,
                        mapping.len(),
                        inner_nbytes,
                    );
                    return Ok(BlockResult::Failure);
                }

                nbytes += inner_nbytes;
            }

            nbytes
        };

        assert_eq!(nbytes as usize, total_size);
        Ok(BlockResult::Success)
    }

    /// Send flush to the file
    fn process_flush(&self) -> Result<BlockResult> {
        let res = unsafe { libc::fdatasync(self.fp.as_raw_fd()) };

        if res == -1 {
            Err(Error::new(ErrorKind::Other, "file flush failed"))
        } else {
            Ok(BlockResult::Success)
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

/*
#[cfg(test)]
mod test {
    use std::collections::VecDeque;
    use std::fs::File;
    use std::io::{Read, Seek, SeekFrom, Write};
    use std::sync::mpsc;
    use std::thread;

    use tempfile::tempdir;

    use crate::block::{BlockOp, BlockDev, BlockReq, PlainBdev, BlockResult};
    use crate::vmm::mapping::{GuardSpace, Prot};
    use crate::common::{GuestAddr, GuestRegion};
    use crate::{DispCtx, Dispatcher, VcpuHdl};

    pub struct TestBlockReq {
        op: BlockOp,
        offset: usize,
        len: usize,
        bufs: VecDeque<GuestRegion>,
        send: mpsc::SyncSender<u64>,
    }

    impl BlockReq for TestBlockReq {
        fn oper(&self) -> BlockOp {
            self.op
        }

        fn offset(&self) -> usize {
            self.offset
        }

        fn next_buf(&mut self) -> Option<GuestRegion> {
            self.bufs.pop_front()
        }

        fn complete(self, _res: BlockResult, _ctx: &DispCtx) {
            self.send.send(0).expect("send failed!");
        }
    }

    impl TestBlockReq {
        fn read(offset: usize, bufs: VecDeque<GuestRegion>) -> (mpsc::Receiver<u64>, TestBlockReq) {
            let (send, recv) = mpsc::sync_channel(1);
            (recv, TestBlockReq {
                op: BlockOp::Read,
                offset,
                len: 0,
                bufs,
                send,
            })
        }

        fn write(offset: usize, bufs: VecDeque<GuestRegion>) -> (mpsc::Receiver<u64>, TestBlockReq) {
            let (send, recv) = mpsc::sync_channel(1);
            (recv, TestBlockReq {
                op: BlockOp::Write,
                offset,
                len: 0,
                bufs,
                send,
            })
        }

        fn flush() -> (mpsc::Receiver<u64>, TestBlockReq) {
            let (send, recv) = mpsc::sync_channel(1);
            (recv, TestBlockReq {
                op: BlockOp::Flush,
                offset: 0,
                len: 0,
                bufs: VecDeque::new(),
                send,
            })
        }
    }


    #[test]
    fn test_plainbdev() {
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

        let bdev =
            PlainBdev::create(file_path.clone(), false)
                .expect("could not create FileBackingStore!");

        let inquiry = bdev.inquire();
        assert_eq!(512, inquiry.total_size * inquiry.block_size as u64);

        /// XXX: Dispatcher context and `bdev.start_dispatch` required for this to work

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
        let (read_recv, read_req) = TestBlockReq::read(0, VecDeque::from(vec![
            GuestRegion(GuestAddr(25), 7),
            GuestRegion(GuestAddr(75), 256),
            GuestRegion(GuestAddr(1350), 128),
            GuestRegion(GuestAddr(2048), 121),
        ]));
        bdev.enqueue(read_req);
        let (flush_recv, flush_req) = TestBlockReq::flush();
        bdev.enqueue(flush_req);

        let _ = read_recv.recv();
        let _ = flush_recv.recv();

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

        let (write_recv, write_req) = TestBlockReq::write(0, VecDeque::from(vec![
            GuestRegion(GuestAddr(3072), 512),
        ]));
        bdev.enqueue(write_req);
        let (flush_recv, flush_req) = TestBlockReq::flush();
        bdev.enqueue(flush_req);

        let _ = write_recv.recv();
        let _ = flush_recv.recv();

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
*/
