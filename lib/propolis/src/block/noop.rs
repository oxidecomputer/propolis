// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::io::{Error, ErrorKind, Result};
use std::num::NonZeroUsize;
use std::sync::Arc;

use crate::block;
use crate::tasks::TaskGroup;
use crate::vmm::MemCtx;

/// Block device backend which discards all writes and returns zeroes for reads.
///
/// This backend is useful for measuring the performance of the emulation
/// stack without any actual storage operations.
pub struct NoopBackend {
    shared_state: Arc<SharedState>,
    block_attach: block::BackendAttachment,

    workers: TaskGroup,
}
struct SharedState {
    info: block::DeviceInfo,
}
impl SharedState {
    async fn processing_loop(&self, wctx: block::AsyncWorkerCtx) {
        while let Some(dreq) = wctx.wait_for_req().await {
            let req = dreq.req();
            if self.info.read_only && req.op.is_write() {
                dreq.complete(block::Result::ReadOnly);
                continue;
            }
            if req.op.is_discard() {
                dreq.complete(block::Result::Unsupported);
                continue;
            }

            let res = match wctx
                .acc_mem()
                .access()
                .and_then(|mem| self.process_request(&req, &mem).ok())
            {
                Some(_) => block::Result::Success,
                None => block::Result::Failure,
            };
            dreq.complete(res);
        }
    }

    fn process_request(
        &self,
        req: &block::Request,
        mem: &MemCtx,
    ) -> std::result::Result<(), &'static str> {
        match req.op {
            block::Operation::Read(_off, _len) => {
                // For reads, write zeros to all regions
                req.regions
                    .iter()
                    .try_fold(0usize, |nread, region| {
                        let map = mem.writable_region(region)?;
                        unsafe {
                            let write_ptr = map.raw_writable()?;
                            let len = map.len();
                            // Write zeros to the destination
                            std::ptr::write_bytes(write_ptr, 0, len);
                            Some(nread + len)
                        }
                    })
                    .ok_or("read failure")?;
            }
            block::Operation::Write(_off, _len) => {
                // For writes, do nothing - just succeed
            }
            block::Operation::Flush => {
                // nothing to do
            }
            block::Operation::Discard(..) => {
                unreachable!("handled in processing_loop()")
            }
        }

        Ok(())
    }
}

impl NoopBackend {
    pub fn create(
        size: u64,
        opts: block::BackendOpts,
        worker_count: NonZeroUsize,
    ) -> Result<Arc<Self>> {
        let block_size = opts.block_size.unwrap_or(block::DEFAULT_BLOCK_SIZE);

        if size == 0 {
            return Err(Error::new(ErrorKind::Other, "size cannot be 0"));
        } else if (size % u64::from(block_size)) != 0 {
            return Err(Error::new(
                ErrorKind::Other,
                format!(
                    "size {} not multiple of block size {}!",
                    size, block_size,
                ),
            ));
        }

        let info = block::DeviceInfo {
            block_size,
            total_size: size / u64::from(block_size),
            read_only: opts.read_only.unwrap_or(false),
            supports_discard: false,
        };
        let block_attach = block::BackendAttachment::new(worker_count, info);

        Ok(Arc::new(Self {
            shared_state: Arc::new(SharedState { info }),
            block_attach,

            workers: TaskGroup::new(),
        }))
    }

    fn spawn_workers(&self) {
        let count = self.block_attach.max_workers().get();
        self.workers.extend((0..count).map(|n| {
            let shared_state = self.shared_state.clone();
            let wctx = self.block_attach.worker(n);
            tokio::spawn(async move {
                let wctx =
                    wctx.activate_async().expect("worker slot is uncontended");
                shared_state.processing_loop(wctx).await
            })
        }))
    }
}

#[async_trait::async_trait]
impl block::Backend for NoopBackend {
    fn attachment(&self) -> &block::BackendAttachment {
        &self.block_attach
    }
    async fn start(&self) -> anyhow::Result<()> {
        self.block_attach.start();
        self.spawn_workers();
        Ok(())
    }
    async fn stop(&self) -> () {
        self.block_attach.stop();
        self.workers.join_all().await;
    }
}
