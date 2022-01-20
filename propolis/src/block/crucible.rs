//! Implement a virtual block device backed by Crucible

use std::collections::VecDeque;
use std::io::{Error, ErrorKind, Result};
use std::net::SocketAddr;
use std::num::NonZeroUsize;
use std::sync::{Arc, Condvar, Mutex};

use super::DeviceInfo;
use crate::block;
use crate::dispatch::{AsyncCtx, DispCtx, Dispatcher, SyncCtx, WakeFn};
use crate::inventory::Entity;
use crate::vmm::SubMapping;

use crucible::CrucibleError;

use tokio::sync::Semaphore;

/// Helper function, because Rust couldn't derive the types
fn map_crucible_error_to_io(x: CrucibleError) -> std::io::Error {
    x.into()
}

pub struct CrucibleBackend {
    guest: Arc<crucible::Guest>,
    block_size: u64,
    sectors: u64,
    read_only: bool,

    driver: Mutex<Option<Arc<SyncDriver>>>,
}

impl CrucibleBackend {
    pub fn create(
        disp: &Dispatcher,
        targets: Vec<SocketAddr>,
        read_only: bool,
        key: Option<String>,
        gen: Option<u64>,
    ) -> Result<Arc<Self>> {
        CrucibleBackend::_create(disp, targets, read_only, key, gen)
            .map_err(map_crucible_error_to_io)
    }

    fn _create(
        disp: &Dispatcher,
        targets: Vec<SocketAddr>,
        read_only: bool,
        key: Option<String>,
        gen: Option<u64>,
    ) -> anyhow::Result<Arc<Self>, crucible::CrucibleError> {
        // spawn Crucible tasks
        let opts = crucible::CrucibleOpts {
            target: targets,
            lossy: false,
            key,
            ..Default::default()
        };
        let guest = Arc::new(crucible::Guest::new());

        let guest_copy = guest.clone();
        tokio::spawn(async move {
            // XXX result eaten here!
            let _ = crucible::up_main(opts, guest_copy).await;
        });

        let mut be = Self {
            guest: guest.clone(),
            block_size: 0,
            sectors: 0,
            read_only,
            driver: Mutex::new(None),
        };

        // XXX Crucible uses std::sync::mpsc::Receiver, not
        // tokio::sync::mpsc::Receiver, so use tokio::task::block_in_place here.
        // Remove that when Crucible changes over to the tokio mpsc.

        // After up_main has returned successfully, wait for active negotiation
        let uuid = tokio::task::block_in_place(|| guest.query_upstairs_uuid())?;

        slog::info!(disp.logger(), "Calling activate for {:?}", uuid);
        tokio::task::block_in_place(|| guest.activate(gen.unwrap_or(0)))?;

        let mut active = false;
        for _ in 0..10 {
            if tokio::task::block_in_place(|| guest.query_is_active())? {
                slog::info!(disp.logger(), "{:?} is active", uuid);
                active = true;
                break;
            }

            tokio::task::block_in_place(|| {
                std::thread::sleep(std::time::Duration::from_secs(2))
            });
        }
        if !active {
            return Err(crucible::CrucibleError::UpstairsInactive);
        }

        // After active negotiation, set sizes
        be.block_size =
            tokio::task::block_in_place(|| guest.query_block_size())?;

        let total_size =
            tokio::task::block_in_place(|| guest.query_total_size())?;

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
        let driver = SyncDriver::new(dev, self.guest.clone());
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
    guest: Arc<crucible::Guest>,
    waiter: block::AsyncWaiter,
}

impl SyncDriver {
    fn new(
        dev: Arc<dyn block::Device>,
        guest: Arc<crucible::Guest>,
    ) -> Arc<Self> {
        let waiter = block::AsyncWaiter::new(dev.as_ref());
        Arc::new(Self {
            cv: Condvar::new(),
            queue: Mutex::new(VecDeque::new()),
            idle_threads: Semaphore::new(0),
            dev,
            guest,
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
                match process_request(self.guest.clone(), &req, &ctx) {
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
    guest: Arc<crucible::Guest>,
    offset: u64,
    len: usize,
    mappings: &Vec<SubMapping>,
) -> std::result::Result<(), CrucibleError> {
    let data = crucible::Buffer::new(len);
    let offset = guest.byte_offset_to_block(offset)?;

    let mut waiter = guest.read(offset, data.clone())?;
    waiter.block_wait()?;

    let mut nwritten = 0;
    for mapping in mappings {
        let slice = &data.as_vec()[nwritten..(nwritten + mapping.len())];
        let inner_nwritten = mapping.write_bytes(slice)?;

        if inner_nwritten != mapping.len() {
            crucible::crucible_bail!(
                IoError,
                "mapping.write_bytes failed! {} vs {}",
                inner_nwritten,
                mapping.len()
            );
        }

        nwritten += mapping.len();
    }

    Ok(())
}

/// Read from all the mappings into vec, and perform one large write to crucible
fn process_write_request(
    guest: Arc<crucible::Guest>,
    offset: u64,
    len: usize,
    mappings: &Vec<SubMapping>,
) -> std::result::Result<(), CrucibleError> {
    let mut vec: Vec<u8> = vec![0; len];

    let mut nread = 0;
    for mapping in mappings {
        let inner_nread =
            mapping.read_bytes(&mut vec[nread..(nread + mapping.len())])?;

        if inner_nread != mapping.len() {
            crucible::crucible_bail!(
                IoError,
                "mapping.read_bytes failed! {} vs {}",
                inner_nread,
                mapping.len(),
            );
        }

        nread += mapping.len();
    }

    let offset = guest.byte_offset_to_block(offset)?;

    let mut waiter = guest.write(offset, crucible::Bytes::from(vec))?;
    waiter.block_wait()?;

    if nread as usize != len {
        crucible::crucible_bail!(IoError, "nread != len! {} vs {}", nread, len,);
    }

    Ok(())
}

/// Send flush to crucible
fn process_flush_request(
    guest: Arc<crucible::Guest>,
) -> std::result::Result<(), CrucibleError> {
    let mut waiter = guest.flush()?;
    waiter.block_wait()?;

    Ok(())
}

fn process_request(
    guest: Arc<crucible::Guest>,
    req: &block::Request,
    ctx: &DispCtx,
) -> Result<()> {
    let mem = ctx.mctx.memctx();
    match req.oper() {
        block::Operation::Read(off) => {
            let maps = req.mappings(&mem).ok_or_else(|| {
                Error::new(ErrorKind::Other, "bad guest region")
            })?;

            process_read_request(guest, off as u64, req.len(), &maps)
                .map_err(map_crucible_error_to_io)?;
        }
        block::Operation::Write(off) => {
            let maps = req.mappings(&mem).ok_or_else(|| {
                Error::new(ErrorKind::Other, "bad guest region")
            })?;

            process_write_request(guest, off as u64, req.len(), &maps)
                .map_err(map_crucible_error_to_io)?;
        }
        block::Operation::Flush(_off, _len) => {
            process_flush_request(guest).map_err(map_crucible_error_to_io)?;
        }
    }

    Ok(())
}
