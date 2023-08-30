// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::io::{Error, ErrorKind, Result};
use std::num::NonZeroUsize;
use std::ptr::NonNull;
use std::sync::Arc;

use crate::block;
use crate::inventory::Entity;
use crate::vmm::MemCtx;

/// Block device backend which uses anonymous memory as its storage.
///
/// While not useful for actually storage data beyond the life of an instance,
/// this backend can be used for measuring how other parts of the emulation
/// stack perform.
pub struct MemAsyncBackend {
    seg: Arc<MmapSeg>,

    workers: NonZeroUsize,
    scheduler: block::Scheduler,

    info: block::DeviceInfo,
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
            seg: Arc::new(seg),

            workers,
            scheduler: block::Scheduler::new(),

            info: block::DeviceInfo {
                block_size,
                total_size: size / block_size as u64,
                read_only: opts.read_only.unwrap_or(false),
            },
        }))
    }
}

fn process_request(
    seg: &MmapSeg,
    read_only: bool,
    req: &block::Request,
    mem: &MemCtx,
) -> std::result::Result<(), &'static str> {
    match req.oper() {
        block::Operation::Read(off) => {
            let maps = req.mappings(mem).ok_or("bad guest region")?;

            let mut nread = 0;
            for map in maps {
                unsafe {
                    let len = map.len();
                    let read_ptr =
                        map.raw_writable().ok_or("wrong protection")?;
                    if !seg.read(off + nread, read_ptr, len) {
                        return Err("read too long");
                    }
                    nread += len;
                };
            }
        }
        block::Operation::Write(off) => {
            if read_only {
                return Err("dev is readonly");
            }

            let maps = req.mappings(mem).ok_or("bad guest region")?;

            let mut nwritten = 0;
            for map in maps {
                unsafe {
                    let len = map.len();
                    let write_ptr =
                        map.raw_readable().ok_or("wrong protection")?;
                    if !seg.write(off + nwritten, write_ptr, len) {
                        return Err("write too long");
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
        self.info
    }

    fn attach(&self, dev: Arc<dyn block::Device>) -> Result<()> {
        self.scheduler.attach(dev);

        for _n in 0..self.workers.get() {
            let seg = self.seg.clone();
            let ro = self.info.read_only;
            let mut worker = self.scheduler.worker();
            tokio::spawn(async move {
                loop {
                    let res = match worker.next().await {
                        None => break,
                        Some((req, mguard)) => {
                            match process_request(&seg, ro, req, &mguard) {
                                Ok(_) => block::Result::Success,
                                Err(_) => block::Result::Failure,
                            }
                        }
                    };
                    worker.complete(res);
                }
            });
        }

        Ok(())
    }

    fn process(&self, _req: &block::Request, _mem: &MemCtx) -> block::Result {
        panic!("request dispatch expected to be done through async logic");
    }
}

impl Entity for MemAsyncBackend {
    fn type_name(&self) -> &'static str {
        "block-memory-async"
    }
    fn start(&self) -> anyhow::Result<()> {
        self.scheduler.start();
        Ok(())
    }
    fn pause(&self) {
        self.scheduler.pause();
    }
    fn resume(&self) {
        self.scheduler.resume();
    }
    fn halt(&self) {
        self.scheduler.halt();
    }
}
