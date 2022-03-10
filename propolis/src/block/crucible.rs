//! Implement a virtual block device backed by Crucible

use std::collections::VecDeque;
use std::io::{Error, ErrorKind, Result};
use std::num::NonZeroUsize;
use std::sync::{Arc, Condvar, Mutex};

use super::DeviceInfo;
use crate::block;
use crate::dispatch::{AsyncCtx, DispCtx, Dispatcher, SyncCtx, WakeFn};
use crate::inventory::Entity;
use crate::vmm::SubMapping;

use crucible::{
    crucible_bail, BlockIO, Buffer, CrucibleError, Volume,
    VolumeConstructionRequest,
};

use tokio::sync::Semaphore;

/// Helper function, because Rust couldn't derive the types
fn map_crucible_error_to_io(x: CrucibleError) -> std::io::Error {
    x.into()
}

pub struct CrucibleBackend {
    block_io: Arc<dyn BlockIO + Send + Sync>,
    block_size: u64,
    sectors: u64,
    read_only: bool,

    driver: Mutex<Option<Arc<SyncDriver>>>,
}

impl CrucibleBackend {
    pub fn create(
        disp: &Dispatcher,
        gen: u64,
        request: VolumeConstructionRequest,
        read_only: bool,
    ) -> Result<Arc<Self>> {
        CrucibleBackend::_create(disp, gen, request, read_only)
            .map_err(map_crucible_error_to_io)
    }

    fn _create(
        disp: &Dispatcher,
        gen: u64,
        request: VolumeConstructionRequest,
        read_only: bool,
    ) -> anyhow::Result<Arc<Self>, crucible::CrucibleError> {
        slog::info!(
            disp.logger(),
            "constructing volume from request {:?}",
            request,
        );

        // XXX Crucible uses std::sync::mpsc::Receiver, not
        // tokio::sync::mpsc::Receiver, so use tokio::task::block_in_place here.
        // Remove that when Crucible changes over to the tokio mpsc.
        let volume = Arc::new(tokio::task::block_in_place(|| {
            Volume::construct(request)
        })?);

        volume.activate(gen)?;

        let mut be = Self {
            block_io: volume.clone(),
            block_size: 0,
            sectors: 0,
            read_only,
            driver: Mutex::new(None),
        };

        // After active negotiation, set sizes
        be.block_size =
            tokio::task::block_in_place(|| volume.get_block_size())?;

        let total_size = tokio::task::block_in_place(|| volume.total_size())?;

        be.sectors = total_size / be.block_size;

        Ok(Arc::new(be))
    }
}

impl block::Backend for CrucibleBackend {
    fn info(&self) -> DeviceInfo {
        DeviceInfo {
            block_size: self.block_size as u32,
            total_size: self.sectors as u64,
            writable: !self.read_only,
        }
    }

    fn attach(&self, dev: Arc<dyn block::Device>, disp: &Dispatcher) {
        let mut driverg = self.driver.lock().unwrap();
        assert!(driverg.is_none());

        // spawn synchronous driver
        let driver =
            SyncDriver::new(dev, self.block_io.clone(), self.read_only);
        driver.spawn(NonZeroUsize::new(8).unwrap(), disp);
        *driverg = Some(driver);
    }
}

impl Entity for CrucibleBackend {
    fn type_name(&self) -> &'static str {
        "block-crucible"
    }
}

struct SyncDriver {
    cv: Condvar,
    queue: Mutex<VecDeque<block::Request>>,
    idle_threads: Semaphore,
    dev: Arc<dyn block::Device>,
    block_io: Arc<dyn BlockIO + Send + Sync>,
    read_only: bool,
    waiter: block::AsyncWaiter,
}

impl SyncDriver {
    fn new(
        dev: Arc<dyn block::Device>,
        block_io: Arc<dyn BlockIO + Send + Sync>,
        read_only: bool,
    ) -> Arc<Self> {
        let waiter = block::AsyncWaiter::new(dev.as_ref());
        Arc::new(Self {
            cv: Condvar::new(),
            queue: Mutex::new(VecDeque::new()),
            idle_threads: Semaphore::new(0),
            dev,
            block_io,
            read_only,
            waiter,
        })
    }

    fn blocking_loop(&self, sctx: &mut SyncCtx) {
        let mut idled = false;
        loop {
            if sctx.check_yield() {
                break;
            }

            let mut guard = self.queue.lock().unwrap();
            if let Some(req) = guard.pop_front() {
                drop(guard);
                idled = false;
                let logger = sctx.log().clone();
                let ctx = sctx.dispctx();
                match process_request(
                    self.block_io.clone(),
                    &req,
                    &ctx,
                    self.read_only,
                ) {
                    Ok(_) => req.complete(block::Result::Success, &ctx),
                    Err(e) => {
                        slog::error!(
                            logger,
                            "{:?} error on req {:?}",
                            e,
                            req.op
                        );
                        req.complete(block::Result::Failure, &ctx)
                    }
                }
            } else {
                // wait until more requests are available
                if !idled {
                    self.idle_threads.add_permits(1);
                    idled = true;
                }
                let _guard = self
                    .cv
                    .wait_while(guard, |g| {
                        // While `sctx.check_yield()` is tempting here, it will
                        // block if this thread goes into a quiesce state,
                        // excluding all others from the queue lock.
                        g.is_empty() && !sctx.pending_reqs()
                    })
                    .unwrap();
            }
        }
    }

    async fn do_scheduling(&self, actx: &AsyncCtx) {
        loop {
            let avail = self.idle_threads.acquire().await.unwrap();
            avail.forget();

            if let Some(req) = self.waiter.next(self.dev.as_ref(), actx).await {
                let mut queue = self.queue.lock().unwrap();
                queue.push_back(req);
                drop(queue);
                self.cv.notify_one();
            }
        }
    }

    fn spawn(self: &Arc<Self>, worker_count: NonZeroUsize, disp: &Dispatcher) {
        for i in 0..worker_count.get() {
            let tself = Arc::clone(self);

            // Configure a waker to help threads to reach their yield points
            // Doing this once (from thread 0) is adequate to wake them all.
            let wake = if i == 0 {
                let tnotify = Arc::downgrade(self);
                Some(Box::new(move |_ctx: &DispCtx| {
                    if let Some(this) = tnotify.upgrade() {
                        let _guard = this.queue.lock().unwrap();
                        this.cv.notify_all();
                    }
                }) as Box<WakeFn>)
            } else {
                None
            };

            let _ = disp
                .spawn_sync(
                    format!("crucible bdev {}", i),
                    Box::new(move |mut sctx| {
                        tself.blocking_loop(&mut sctx);
                    }),
                    wake,
                )
                .unwrap();
        }

        // TODO: do we need the task for later?
        let sched_self = Arc::clone(self);
        let actx = disp.async_ctx();
        let _sched_task = tokio::spawn(async move {
            let _ = sched_self.do_scheduling(&actx).await;
        });
    }
}

/// Perform one large read from crucible, and write from data into mappings
fn process_read_request(
    block_io: Arc<dyn BlockIO + Send + Sync>,
    offset: u64,
    len: usize,
    mappings: &Vec<SubMapping>,
) -> std::result::Result<(), CrucibleError> {
    let data = Buffer::new(len);
    let offset = block_io.byte_offset_to_block(offset)?;

    let mut waiter = block_io.read(offset, data.clone())?;
    waiter.block_wait()?;

    let mut nwritten = 0;
    for mapping in mappings {
        nwritten += mapping.write_bytes(
            &data.as_vec()[nwritten..(nwritten + mapping.len())],
        )?;
    }

    if nwritten as usize != len {
        crucible_bail!(IoError, "nwritten != len! {} vs {}", nwritten, len);
    }

    Ok(())
}

/// Read from all the mappings into vec, and perform one large write to crucible
fn process_write_request(
    block_io: Arc<dyn BlockIO + Send + Sync>,
    offset: u64,
    len: usize,
    mappings: &Vec<SubMapping>,
) -> std::result::Result<(), CrucibleError> {
    let mut vec: Vec<u8> = vec![0; len];

    let mut nread = 0;
    for mapping in mappings {
        nread +=
            mapping.read_bytes(&mut vec[nread..(nread + mapping.len())])?;
    }

    let offset = block_io.byte_offset_to_block(offset)?;

    let mut waiter = block_io.write(offset, crucible::Bytes::from(vec))?;
    waiter.block_wait()?;

    if nread as usize != len {
        crucible_bail!(IoError, "nread != len! {} vs {}", nread, len);
    }

    Ok(())
}

/// Send flush to crucible
fn process_flush_request(
    block_io: Arc<dyn BlockIO + Send + Sync>,
) -> std::result::Result<(), CrucibleError> {
    let mut waiter = block_io.flush(None)?;
    waiter.block_wait()?;

    Ok(())
}

fn process_request(
    block_io: Arc<dyn BlockIO + Send + Sync>,
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

            process_read_request(block_io, off as u64, req.len(), &maps)
                .map_err(map_crucible_error_to_io)?;
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

            process_write_request(block_io, off as u64, req.len(), &maps)
                .map_err(map_crucible_error_to_io)?;
        }
        block::Operation::Flush(_off, _len) => {
            process_flush_request(block_io)
                .map_err(map_crucible_error_to_io)?;
        }
    }

    Ok(())
}
