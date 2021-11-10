use std::collections::VecDeque;
use std::fs::{metadata, File, OpenOptions};
use std::io::{Error, ErrorKind, Result};
use std::num::NonZeroUsize;
use std::os::unix::io::AsRawFd;
use std::path::Path;
use std::sync::{Arc, Condvar, Mutex};

use super::DeviceInfo;
use crate::block;
use crate::dispatch::{AsyncCtx, DispCtx, Dispatcher, SyncCtx, WakeFn};
use crate::inventory::Entity;
use crate::vmm::MappingExt;

use tokio::sync::Semaphore;

// XXX: completely arb for now
const MAX_WORKERS: usize = 32;

/// Standard [`BlockDev`] implementation.
pub struct FileBackend {
    fp: Arc<File>,

    driver: Mutex<Option<Arc<Driver>>>,
    worker_count: NonZeroUsize,

    is_ro: bool,
    block_size: usize,
    sectors: usize,
}

impl FileBackend {
    /// Creates a new block device from a device at `path`.
    pub fn create(
        path: impl AsRef<Path>,
        readonly: bool,
        worker_count: NonZeroUsize,
    ) -> Result<Arc<Self>> {
        if worker_count.get() > MAX_WORKERS {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "too many workers",
            ));
        }
        let p: &Path = path.as_ref();

        let meta = metadata(p)?;
        let is_ro = readonly || meta.permissions().readonly();

        let fp = OpenOptions::new().read(true).write(!is_ro).open(p)?;
        let len = fp.metadata().unwrap().len() as usize;

        let this = Self {
            fp: Arc::new(fp),

            driver: Mutex::new(None),
            worker_count,

            is_ro,
            block_size: 512,
            sectors: len / 512,
        };

        Ok(Arc::new(this))
    }
}
impl block::Backend for FileBackend {
    fn info(&self) -> DeviceInfo {
        DeviceInfo {
            block_size: self.block_size as u32,
            total_size: self.sectors as u64,
            writable: !self.is_ro,
        }
    }

    fn attach(&self, dev: Arc<dyn block::Device>, disp: &Dispatcher) {
        let mut driverg = self.driver.lock().unwrap();
        assert!(driverg.is_none());

        let driver = Driver::new(self.fp.clone(), dev);
        driver.spawn(self.worker_count, disp);
        *driverg = Some(driver);
    }
}
impl Entity for FileBackend {
    fn type_name(&self) -> &'static str {
        "block-file"
    }
}

struct Driver {
    fp: Arc<File>,
    cv: Condvar,
    queue: Mutex<VecDeque<block::Request>>,
    idle_threads: Semaphore,
    dev: Arc<dyn block::Device>,
    waiter: block::AsyncWaiter,
}
impl Driver {
    fn new(fp: Arc<File>, dev: Arc<dyn block::Device>) -> Arc<Self> {
        let waiter = block::AsyncWaiter::new(dev.as_ref());
        Arc::new(Self {
            fp,
            cv: Condvar::new(),
            queue: Mutex::new(VecDeque::new()),
            idle_threads: Semaphore::new(0),
            dev,
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
                let ctx = sctx.dispctx();
                match process_request(&self.fp, &req, &ctx) {
                    Ok(_) => req.complete(block::Result::Success, &ctx),
                    Err(_) => req.complete(block::Result::Failure, &ctx),
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
                    format!("file bdev {}", i),
                    Box::new(move |mut sctx| {
                        tself.blocking_loop(&mut sctx);
                    }),
                    wake,
                )
                .unwrap();
        }

        let sched_self = Arc::clone(self);
        let actx = disp.async_ctx();
        let sched_task = tokio::spawn(async move {
            sched_self.do_scheduling(&actx).await;
        });
        // TODO: do we need to manipulate the task later?
        disp.track(sched_task);
    }
}

fn process_request(
    fp: &File,
    req: &block::Request,
    ctx: &DispCtx,
) -> Result<()> {
    let mem = ctx.mctx.memctx();
    match req.oper() {
        block::Operation::Read(off) => {
            let maps = req.mappings(&mem).ok_or_else(|| {
                Error::new(ErrorKind::Other, "bad guest region")
            })?;

            let nbytes = maps.preadv(fp.as_raw_fd(), off as i64)?;
            if nbytes != req.len() {
                return Err(Error::new(ErrorKind::Other, "bad read length"));
            }
        }
        block::Operation::Write(off) => {
            let maps = req.mappings(&mem).ok_or_else(|| {
                Error::new(ErrorKind::Other, "bad guest region")
            })?;

            let nbytes = maps.pwritev(fp.as_raw_fd(), off as i64)?;
            if nbytes != req.len() {
                return Err(Error::new(ErrorKind::Other, "bad write length"));
            }
        }
        block::Operation::Flush(_off, _len) => {
            fp.sync_data()?;
        }
    }
    Ok(())
}
