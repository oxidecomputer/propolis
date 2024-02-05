// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::io::{Error, ErrorKind, Result};
use std::num::NonZeroUsize;
use std::ptr::NonNull;
use std::sync::Arc;

use crate::accessors::MemAccessor;
use crate::block;
use crate::vmm::MemCtx;

/// Block device backend which uses anonymous memory as its storage.
///
/// While not useful for actually storage data beyond the life of an instance,
/// this backend can be used for measuring how other parts of the emulation
/// stack perform.
pub struct MemAsyncBackend {
    work_state: Arc<WorkingState>,

    workers: NonZeroUsize,
}
struct WorkingState {
    attachment: block::backend::Attachment,
    seg: MmapSeg,
    info: block::DeviceInfo,
}
impl WorkingState {
    async fn processing_loop(&self, acc_mem: MemAccessor) {
        while let Some(req) = self.attachment.wait_for_req().await {
            if self.info.read_only && req.oper().is_write() {
                req.complete(block::Result::ReadOnly);
                continue;
            }
            let res = match acc_mem
                .access()
                .and_then(|mem| self.process_request(&req, &mem).ok())
            {
                Some(_) => block::Result::Success,
                None => block::Result::Failure,
            };
            req.complete(res);
        }
    }

    fn process_request(
        &self,
        req: &block::Request,
        mem: &MemCtx,
    ) -> std::result::Result<(), &'static str> {
        let seg = &self.seg;
        match req.oper() {
            block::Operation::Read(off, _len) => {
                let maps = req.mappings(mem).ok_or("bad mapping")?;

                let mut nread = 0;
                for map in maps {
                    unsafe {
                        let len = map.len();
                        let read_ptr = map
                            .raw_writable()
                            .ok_or("expected writable mapping")?;
                        if !seg.read(off + nread, read_ptr, len) {
                            return Err("failed mem read");
                        }
                        nread += len;
                    };
                }
            }
            block::Operation::Write(off, _len) => {
                let maps = req.mappings(mem).ok_or("bad mapping")?;

                let mut nwritten = 0;
                for map in maps {
                    unsafe {
                        let len = map.len();
                        let write_ptr = map
                            .raw_readable()
                            .ok_or("expected readable mapping")?;
                        if !seg.write(off + nwritten, write_ptr, len) {
                            return Err("failed mem write");
                        }
                        nwritten += len;
                    };
                }
            }
            block::Operation::Flush => {
                // nothing to do
            }
        }

        Ok(())
    }
}

impl MemAsyncBackend {
    pub fn create(
        size: u64,
        opts: block::BackendOpts,
        workers: NonZeroUsize,
    ) -> Result<Arc<Self>> {
        let block_size = opts.block_size.unwrap_or(block::DEFAULT_BLOCK_SIZE);

        if size == 0 {
            return Err(Error::new(ErrorKind::Other, "size cannot be 0"));
        } else if (size % block_size as u64) != 0 {
            return Err(Error::new(
                ErrorKind::Other,
                format!(
                    "size {} not multiple of block size {}!",
                    size, block_size,
                ),
            ));
        }

        let seg = MmapSeg::new(size as usize)?;

        Ok(Arc::new(Self {
            work_state: Arc::new(WorkingState {
                attachment: block::backend::Attachment::new(),
                info: block::DeviceInfo {
                    block_size,
                    total_size: size / block_size as u64,
                    read_only: opts.read_only.unwrap_or(false),
                },
                seg,
            }),

            workers,
        }))
    }

    fn spawn_workers(&self) {
        for n in 0..self.workers.get() {
            let worker_state = self.work_state.clone();
            let worker_acc =
                self.work_state.attachment.accessor_mem(|acc_mem| {
                    acc_mem
                        .expect("backend is attached")
                        .child(Some(format!("worker {n}")))
                });
            tokio::spawn(async move {
                worker_state.processing_loop(worker_acc).await
            });
        }
    }
}

struct MmapSeg(NonNull<u8>, usize);
impl MmapSeg {
    fn new(size: usize) -> Result<Self> {
        let ptr = unsafe {
            libc::mmap(
                core::ptr::null_mut(),
                size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANON,
                -1,
                0,
            )
        };

        if ptr == libc::MAP_FAILED {
            return Err(Error::last_os_error());
        }
        Ok(Self(NonNull::new(ptr as *mut u8).unwrap(), size))
    }
    unsafe fn write(&self, off: usize, data: *const u8, sz: usize) -> bool {
        if (off + sz) > self.1 {
            return false;
        }

        self.0.as_ptr().add(off).copy_from_nonoverlapping(data, sz);
        true
    }
    unsafe fn read(&self, off: usize, data: *mut u8, sz: usize) -> bool {
        if (off + sz) > self.1 {
            return false;
        }

        self.0.as_ptr().add(off).copy_to_nonoverlapping(data, sz);
        true
    }
}
impl Drop for MmapSeg {
    fn drop(&mut self) {
        unsafe {
            libc::munmap(self.0.as_ptr() as *mut libc::c_void, self.1);
        }
    }
}
// Safety: The consumer is allowed to make their own pointer mistakes
unsafe impl Send for MmapSeg {}
unsafe impl Sync for MmapSeg {}

impl block::Backend for MemAsyncBackend {
    fn info(&self) -> block::DeviceInfo {
        self.work_state.info
    }
    fn attachment(&self) -> &block::backend::Attachment {
        &self.work_state.attachment
    }
    fn start(&self) -> anyhow::Result<()> {
        self.work_state.attachment.start();
        self.spawn_workers();
        Ok(())
    }
}
