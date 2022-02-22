use std::collections::VecDeque;
use std::io::{Error, ErrorKind, Result};
use std::num::NonZeroUsize;
use std::sync::{Arc, Condvar, Mutex};

use super::DeviceInfo;
use crate::block;
use crate::dispatch::{AsyncCtx, DispCtx, Dispatcher, SyncCtx, WakeFn};
use crate::inventory::Entity;
use crate::vmm::SubMapping;

use tokio::sync::Semaphore;

pub struct InMemoryBackend {
    bytes: Arc<Mutex<Vec<u8>>>,

    driver: Mutex<Option<Arc<Driver>>>,
    worker_count: NonZeroUsize,

    is_ro: bool,
    block_size: usize,
    sectors: usize,
}

impl InMemoryBackend {
    pub fn create(
        bytes: Vec<u8>,
        is_ro: bool,
        block_size: usize,
    ) -> Result<Arc<Self>> {
        match block_size {
            512 | 4096 => {
                // ok
            }
            _ => {
                return Err(std::io::Error::new(
                    ErrorKind::Other,
                    format!("unsupported block size {}!", block_size,),
                ));
            }
        }

        let len = bytes.len();

        if (len % block_size) != 0 {
            return Err(std::io::Error::new(
                ErrorKind::Other,
                format!(
                    "in-memory bytes length {} not multiple of block size {}!",
                    len, block_size,
                ),
            ));
        }

        let this = Self {
            bytes: Arc::new(Mutex::new(bytes)),

            driver: Mutex::new(None),
            worker_count: NonZeroUsize::new(1).unwrap(),

            is_ro,
            block_size,
            sectors: len / block_size,
        };

        Ok(Arc::new(this))
    }
}

impl block::Backend for InMemoryBackend {
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

        let driver = Driver::new(self.bytes.clone(), dev);
        driver.spawn(self.worker_count, disp);
        *driverg = Some(driver);
    }
}

impl Entity for InMemoryBackend {
    fn type_name(&self) -> &'static str {
        "in-memory"
    }
}

struct Driver {
    bytes: Arc<Mutex<Vec<u8>>>,
    cv: Condvar,
    queue: Mutex<VecDeque<block::Request>>,
    idle_threads: Semaphore,
    dev: Arc<dyn block::Device>,
    waiter: block::AsyncWaiter,
}

impl Driver {
    fn new(
        bytes: Arc<Mutex<Vec<u8>>>,
        dev: Arc<dyn block::Device>,
    ) -> Arc<Self> {
        let waiter = block::AsyncWaiter::new(dev.as_ref());
        Arc::new(Self {
            bytes,
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
                match process_request(&self.bytes, &req, &ctx) {
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
                    format!("mem bdev {}", i),
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

/// Read from bytes into guest memory
fn process_read_request(
    bytes: &Arc<Mutex<Vec<u8>>>,
    offset: u64,
    len: usize,
    mappings: &Vec<SubMapping>,
) -> Result<()> {
    let bytes = bytes.lock().unwrap();

    let start = offset as usize;
    let end = offset as usize + len;

    if start >= bytes.len() || end >= bytes.len() {
        return Err(std::io::Error::new(
            ErrorKind::InvalidInput,
            format!(
                "invalid offset {} and len {} when bytes len is {}",
                offset,
                len,
                bytes.len(),
            ),
        ));
    }

    let data = &bytes[start..end];

    let mut nwritten = 0;
    for mapping in mappings {
        nwritten +=
            mapping.write_bytes(&data[nwritten..(nwritten + mapping.len())])?;
    }

    Ok(())
}

/// Write from guest memory into bytes
fn process_write_request(
    bytes: &Arc<Mutex<Vec<u8>>>,
    offset: u64,
    len: usize,
    mappings: &Vec<SubMapping>,
) -> Result<()> {
    let mut bytes = bytes.lock().unwrap();

    let start = offset as usize;
    let end = offset as usize + len;

    if start >= bytes.len() || end >= bytes.len() {
        return Err(std::io::Error::new(
            ErrorKind::InvalidInput,
            format!(
                "invalid offset {} and len {} when bytes len is {}",
                offset,
                len,
                bytes.len(),
            ),
        ));
    }

    let data = &mut bytes[start..end];

    let mut nread = 0;
    for mapping in mappings {
        nread +=
            mapping.read_bytes(&mut data[nread..(nread + mapping.len())])?;
    }

    Ok(())
}

fn process_request(
    bytes: &Arc<Mutex<Vec<u8>>>,
    req: &block::Request,
    ctx: &DispCtx,
) -> Result<()> {
    let mem = ctx.mctx.memctx();
    match req.oper() {
        block::Operation::Read(off) => {
            let maps = req.mappings(&mem).ok_or_else(|| {
                Error::new(ErrorKind::Other, "bad guest region")
            })?;

            process_read_request(bytes, off as u64, req.len(), &maps)?;
        }
        block::Operation::Write(off) => {
            let maps = req.mappings(&mem).ok_or_else(|| {
                Error::new(ErrorKind::Other, "bad guest region")
            })?;

            process_write_request(bytes, off as u64, req.len(), &maps)?;
        }
        block::Operation::Flush(_off, _len) => {
            // nothing to do
        }
    }

    Ok(())
}
