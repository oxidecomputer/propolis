//! Implement a virtual block device backed by Crucible

use bytes::Bytes;
use crucible::{Buffer, Guest};
use std::collections::VecDeque;
use std::io::{Error, ErrorKind, Result};
use std::net::SocketAddrV4;
use std::sync::{Arc, Condvar, Mutex, Weak};

use crate::block::{BlockDev, BlockInquiry, BlockOp, BlockReq, BlockResult};
use crate::common::GuestRegion;
use crate::vmm::{MemCtx, SubMapping};
use crate::{DispCtx, Dispatcher};

pub struct CrucibleBlockDev<R: BlockReq> {
    guest: Arc<Guest>,
    block_size: u64,
    total_size: u64,
    read_only: bool,
    reqs: Mutex<VecDeque<R>>,
    cond: Condvar,
}

impl<R: BlockReq> CrucibleBlockDev<R> {
    fn set_sizes(&mut self) {
        self.block_size = self.guest.query_block_size();
        self.total_size = self.guest.query_total_size();
    }

    pub fn new(
        targets: Vec<SocketAddrV4>,
        runtime: &Option<tokio::runtime::Runtime>,
        read_only: bool,
    ) -> Result<Arc<CrucibleBlockDev<R>>> {
        let opts = crucible::CrucibleOpts { target: targets, lossy: false };

        let mut blockdev = Self {
            guest: Arc::new(Guest::new()),
            block_size: 0,
            total_size: 0,
            read_only,
            reqs: Mutex::new(VecDeque::new()),
            cond: Condvar::new(),
        };

        let task = crucible::up_main(opts, blockdev.guest.clone());
        match runtime {
            Some(runtime) => {
                runtime.spawn(task);
                println!("Crucible runtime spawned from CrucibleBlockDev::new");
            }
            None => {
                tokio::spawn(task);
                println!(
                    "Crucible runtime spawned from CrucibleBlockDev::new (through tokio::spawn)"
                );
            }
        }

        // After up_main has returned successfully, these can be queried
        blockdev.set_sizes();

        Ok(Arc::new(blockdev))
    }

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

    /// Delegate a block device read or write to crucible.
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
                    mem.writable_region(buf)
                } else {
                    mem.readable_region(buf)
                }
            })
            .collect::<Option<Vec<SubMapping>>>()
            .ok_or_else(|| {
                Error::new(ErrorKind::Other, "getting a region failed!")
            })?;

        let total_size: usize = mappings.iter().map(|x| x.len()).sum();

        if is_read {
            // Perform one large read from crucible, and write from data into mappings
            let data = Buffer::new(total_size);

            let mut waiter = self.guest.read(offset as u64, data.clone());
            waiter.block_wait();

            let mut nwritten = 0;

            for mapping in mappings {
                let slice =
                    &data.as_vec()[nwritten..(nwritten + mapping.len())];
                let inner_nwritten = mapping.write_bytes(slice)?;

                if inner_nwritten != mapping.len() {
                    return Err(Error::new(
                        ErrorKind::Other,
                        format!(
                            "mapping.write_bytes failed! {} vs {}",
                            inner_nwritten,
                            mapping.len()
                        ),
                    ));
                }

                nwritten += mapping.len();
            }

            assert_eq!(nwritten as usize, total_size);
            Ok(BlockResult::Success)
        } else {
            // Read from all the mappings into vec, and perform one large write to crucible
            let mut vec: Vec<u8> = vec![0; total_size];

            let mut nread = 0;
            for mapping in mappings {
                let inner_nread = mapping
                    .read_bytes(&mut vec[nread..(nread + mapping.len())])?;

                if inner_nread != mapping.len() {
                    return Err(Error::new(
                        ErrorKind::Other,
                        format!(
                            "mapping.read_bytes failed! {} vs {}",
                            inner_nread,
                            mapping.len()
                        ),
                    ));
                }

                nread += mapping.len();
            }

            let mut waiter = self.guest.write(offset as u64, Bytes::from(vec));
            waiter.block_wait();

            assert_eq!(nread as usize, total_size);
            Ok(BlockResult::Success)
        }
    }

    /// Send flush to crucible
    fn process_flush(&self) -> Result<BlockResult> {
        let mut waiter = self.guest.flush();
        waiter.block_wait();

        Ok(BlockResult::Success)
    }
}

impl<R: BlockReq> BlockDev<R> for CrucibleBlockDev<R> {
    /// Enqueues a [`BlockReq`] to the underlying device.
    fn enqueue(&self, req: R) {
        self.reqs.lock().unwrap().push_back(req);
        self.cond.notify_all();
    }

    /// Requests metadata about the block device.
    fn inquire(&self) -> BlockInquiry {
        BlockInquiry {
            total_size: self.total_size / self.block_size,
            block_size: self.block_size as u32,
            writable: !self.read_only,
        }
    }

    /// Spawns a new thread named `name` on the dispatcher `disp` which
    /// begins processing incoming requests.
    fn start_dispatch(self: Arc<Self>, name: String, disp: &Dispatcher) {
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
