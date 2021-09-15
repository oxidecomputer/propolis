use std::collections::VecDeque;
use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use crate::chardev::{BlockingSource, Sink, Source};
use crate::dispatch::{AsyncCtx, DispCtx};

use tokio::sync::Notify;
use tokio::time::sleep;

pub struct Params {
    pub poll_interval: Duration,
    pub poll_miss_thresh: usize,
    pub buf_size: NonZeroUsize,
}
struct SourceInner {
    buf: Vec<u8>,
    last_poll: Option<Instant>,
}
impl SourceInner {
    fn is_full(&self) -> bool {
        self.buf.len() == self.buf.capacity()
    }
}
pub struct SourceBuffer {
    data_ready: Notify,
    inner: Mutex<SourceInner>,
    poll_active: AtomicBool,
    params: Params,
}
impl SourceBuffer {
    pub fn new(params: Params) -> Arc<Self> {
        let this = Self {
            data_ready: Notify::new(),
            inner: Mutex::new(SourceInner {
                buf: Vec::with_capacity(params.buf_size.get()),
                last_poll: None,
            }),
            poll_active: AtomicBool::new(true),
            params,
        };
        Arc::new(this)
    }

    pub fn attach(self: &Arc<Self>, source: &dyn Source) {
        let this = Arc::clone(self);
        source.set_autodiscard(false);
        source.set_notifier(Some(Box::new(move |s, ctx| {
            this.notify(s, ctx);
        })));
    }

    pub async fn read(
        &self,
        buf: &mut [u8],
        source: &dyn Source,
        actx: &mut AsyncCtx,
    ) -> Option<usize> {
        if buf.is_empty() {
            return Some(0);
        }
        if self.poll_active.load(Ordering::Acquire) {
            let _ = self.leading_delay().await;
        }
        loop {
            let ctx = actx.dispctx().await?;
            let nread = self.read_data(buf, source, &ctx);
            if nread > 0 {
                return Some(nread);
            }
            drop(ctx);

            self.wait().await;
        }
    }

    /// If we are polling this Source and the buffer is not full, we may want to
    /// wait to let it fill further so we make fewer trips back and forth.
    async fn leading_delay(&self) -> Option<Duration> {
        let last_poll = {
            let inner = self.inner.lock().unwrap();
            // No delay if already full
            if inner.is_full() {
                return None;
            }
            let last = inner.last_poll;
            drop(inner);
            last
        };

        if let Some(since) = last_poll.map(|t| Instant::now().duration_since(t))
        {
            if let Some(diff) = self.params.poll_interval.checked_sub(since) {
                tokio::select! {
                    _ = sleep(diff) => {},
                    _ = self.data_ready.notified() => {},
                };
                return Some(diff);
            }
        }
        None
    }

    async fn wait(&self) {
        self.poll_active.store(true, Ordering::Release);
        let mut misses = 0;
        loop {
            tokio::select! {
                _ = sleep(self.params.poll_interval) => {
                    let mut inner = self.inner.lock().unwrap();
                    if inner.buf.is_empty() {
                        inner.last_poll = Some(Instant::now());
                        misses += 1;
                        if misses > self.params.poll_miss_thresh {
                            self.poll_active.store(false, Ordering::Release);
                            break;
                        }
                    } else {
                        return;
                    }
                },
                _ = self.data_ready.notified() => {
                    return;
                },
            };
        }

        // We have exceeded the miss threshold
        self.data_ready.notified().await;
    }

    pub fn read_data(
        &self,
        buf: &mut [u8],
        source: &dyn Source,
        ctx: &DispCtx,
    ) -> usize {
        let mut inner = self.inner.lock().unwrap();
        let mut copied = copy_and_consume(&mut inner.buf, buf);
        // Can also attempt to read direct from the Source
        if copied < buf.len() {
            if let Some(b) = source.read(ctx) {
                buf[copied] = b;
                copied += 1;
            }
        }
        inner.last_poll = Some(Instant::now());
        copied
    }

    fn notify(&self, source: &dyn Source, ctx: &DispCtx) {
        if self.poll_active.load(Ordering::Acquire) {
            let mut inner = self.inner.lock().unwrap();
            if !inner.is_full() {
                if let Some(c) = source.read(ctx) {
                    inner.buf.push(c);
                }
                // If the buffer is not full and polling is still active, elide
                // the notification to the Source consumer.
                if !inner.is_full() && self.poll_active.load(Ordering::Acquire)
                {
                    return;
                }
            }
        }
        self.data_ready.notify_one();
    }
}

struct SinkInner {
    buf: VecDeque<u8>,
    wait_empty: bool,
}

pub struct SinkBuffer {
    notify: Notify,
    inner: Mutex<SinkInner>,
}

impl SinkBuffer {
    pub fn new(size: NonZeroUsize) -> Arc<Self> {
        let this = Self {
            notify: Notify::new(),
            inner: Mutex::new(SinkInner {
                buf: VecDeque::with_capacity(size.get()),
                wait_empty: false,
            }),
        };
        Arc::new(this)
    }

    pub fn attach(self: &Arc<Self>, sink: &dyn Sink) {
        let this = Arc::clone(self);
        sink.set_notifier(Some(Box::new(move |s, ctx| {
            this.notify(s, ctx);
        })));
    }

    pub async fn wait_empty(&self) {
        loop {
            {
                let mut inner = self.inner.lock().unwrap();
                if inner.buf.is_empty() {
                    inner.wait_empty = false;
                    return;
                }
                inner.wait_empty = true;
            }
            self.notify.notified().await;
        }
    }

    pub async fn populate(
        &self,
        sink: &dyn Sink,
        mut data: &[u8],
        actx: &mut AsyncCtx,
    ) {
        if data.is_empty() {
            return;
        }
        loop {
            let was_empty = {
                let mut inner = self.inner.lock().unwrap();
                let was_empty = inner.buf.is_empty();
                while inner.buf.capacity() > inner.buf.len() {
                    if let Some((c, rest)) = data.split_first() {
                        inner.buf.push_back(*c);
                        data = rest;
                    } else {
                        break;
                    }
                }
                was_empty
            };

            // If the buffer started empty, try to kick the sink into
            // accepting (more) data.
            if was_empty {
                if let Some(ctx) = actx.dispctx().await {
                    let mut inner = self.inner.lock().unwrap();
                    if let Some(c) = inner.buf.front() {
                        if sink.write(*c, &ctx) {
                            inner.buf.pop_front();
                        }
                    }
                }
            }

            //
            if !data.is_empty() {
                // wait for more room to push data (without too many wake-ups
                self.wait_empty().await;
            } else {
                return;
            }
        }
    }

    fn notify(&self, sink: &dyn Sink, ctx: &DispCtx) {
        let mut inner = self.inner.lock().unwrap();
        while let Some(c) = inner.buf.pop_front() {
            if !sink.write(c, ctx) {
                inner.buf.push_front(c);
                break;
            }
        }
        if inner.buf.is_empty() || !inner.wait_empty {
            self.notify.notify_one();
        }
    }
}

pub struct BlockingParams {
    pub poll_interval: Duration,
    pub poll_miss_thresh: usize,
    pub buf_size: NonZeroUsize,
}
struct BlockingSourceInner {
    buf: Vec<u8>,
    last_poll: Option<Instant>,
}
impl BlockingSourceInner {
    fn is_full(&self) -> bool {
        self.buf.len() == self.buf.capacity()
    }
}
pub struct BlockingSourceBuffer {
    data_ready: Notify,
    consume_cv: Condvar,
    inner: Mutex<BlockingSourceInner>,
    poll_active: AtomicBool,
    params: BlockingParams,
}
impl BlockingSourceBuffer {
    pub fn new(params: BlockingParams) -> Arc<Self> {
        let this = Self {
            data_ready: Notify::new(),
            consume_cv: Condvar::new(),
            inner: Mutex::new(BlockingSourceInner {
                buf: Vec::with_capacity(params.buf_size.get()),
                last_poll: None,
            }),
            poll_active: AtomicBool::new(false),
            params,
        };
        Arc::new(this)
    }

    pub fn attach(self: &Arc<Self>, source: &dyn BlockingSource) {
        let this = Arc::clone(self);
        source.set_consumer(Some(Box::new(move |data, ctx| {
            this.consume(data, ctx);
        })));
    }

    pub async fn read(&self, buf: &mut [u8]) -> Option<usize> {
        if buf.is_empty() {
            return Some(0);
        }
        if self.poll_active.load(Ordering::Relaxed) {
            let _ = self.leading_delay().await;
        }
        loop {
            let nread = {
                let mut inner = self.inner.lock().unwrap();
                inner.last_poll = Some(Instant::now());
                let nread = copy_and_consume(&mut inner.buf, buf);
                self.consume_cv.notify_one();
                nread
            };
            if nread > 0 {
                return Some(nread);
            }

            self.wait_for_data().await;
        }
    }

    /// If we are polling this Source and the buffer is not full, we may want to
    /// wait to let it fill further so we make fewer trips back and forth.
    async fn leading_delay(&self) -> Option<Duration> {
        let last_poll = {
            let inner = self.inner.lock().unwrap();
            // No delay if already full
            if inner.is_full() {
                return None;
            }
            let last = inner.last_poll;
            drop(inner);
            last
        };

        if let Some(since) = last_poll.map(|t| Instant::now().duration_since(t))
        {
            if let Some(diff) = self.params.poll_interval.checked_sub(since) {
                tokio::select! {
                    _ = sleep(diff) => {},
                    _ = self.data_ready.notified() => {},
                };
                return Some(diff);
            }
        }
        None
    }

    async fn wait_for_data(&self) {
        self.poll_active.store(true, Ordering::Release);
        let mut misses = 0;
        loop {
            tokio::select! {
                _ = sleep(self.params.poll_interval) => {
                    let mut inner = self.inner.lock().unwrap();
                    if inner.buf.is_empty() {
                        inner.last_poll = Some(Instant::now());
                        misses += 1;
                        if misses > self.params.poll_miss_thresh {
                            self.poll_active.store(false, Ordering::Release);
                            break;
                        }
                    } else {
                        return;
                    }
                },
                _ = self.data_ready.notified() => {
                    return;
                },
            };
        }

        // We have exceeded the miss threshold
        self.data_ready.notified().await;
    }

    fn consume(&self, mut data: &[u8], _ctx: &DispCtx) {
        let mut inner = self.inner.lock().unwrap();
        while !data.is_empty() {
            if inner.is_full() {
                self.data_ready.notify_one();
                // TODO: What guarantees do we want make about the poller
                // vacating space in the buffer in a timely fashion?  This is
                // particularly relevant during operations like quiesce.
                inner =
                    self.consume_cv.wait_while(inner, |i| i.is_full()).unwrap();
            }
            let old_len = inner.buf.len();
            let copy_len =
                usize::min(data.len(), inner.buf.capacity() - old_len);
            inner.buf.extend_from_slice(&data[..copy_len]);
            let (_consumed, remain) = data.split_at(copy_len);
            data = remain;
        }

        if !inner.is_full() {
            if self.poll_active.load(Ordering::Acquire) {
                // The buffer is not full and we are being polled, so elide the
                // wake-up for now.
                return;
            }
        }
        drop(inner);
        self.data_ready.notify_one();
    }
}

/// Copy available data from a Vec.  Any remaining data will be copied to the
/// front of the Vec, truncating the vacated space without altering its allocated
/// capacity.
fn copy_and_consume(src: &mut Vec<u8>, dest: &mut [u8]) -> usize {
    if src.is_empty() || dest.is_empty() {
        0
    } else {
        let old_len = src.len();
        let copy_len = usize::min(dest.len(), old_len);
        dest[..copy_len].copy_from_slice(&src[..copy_len]);
        if copy_len != old_len {
            src.copy_within(copy_len.., 0);
        }
        src.truncate(old_len - copy_len);
        copy_len
    }
}
