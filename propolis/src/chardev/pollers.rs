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

    /// Read data from Source and/or its associated buffer.
    ///
    /// # Cancel safety
    ///
    /// This method is cancel safe.  It can be used in `tokio::select!` and if
    /// cancelled, it is guaranteed that no data will have been read.
    pub async fn read(
        &self,
        buf: &mut [u8],
        source: &dyn Source,
        actx: &AsyncCtx,
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
impl SinkInner {
    fn is_full(&self) -> bool {
        self.buf.len() == self.buf.capacity()
    }
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

    /// Write data into the Sink and/or its associated buffer.
    ///
    /// # Cancel safety
    ///
    /// This method is cancel safe.  It can be used in `tokio::select!` and if
    /// cancelled, it is guaranteed that no data will have been written.
    pub async fn write(
        &self,
        mut data: &[u8],
        sink: &dyn Sink,
        actx: &AsyncCtx,
    ) -> Option<usize> {
        if data.is_empty() {
            return Some(0);
        }
        loop {
            {
                let ctx = actx.dispctx().await?;
                let mut inner = self.inner.lock().unwrap();
                let mut nwritten = 0;

                // If the buffer started empty, try to kick the sink into
                // accepting data.
                if inner.buf.is_empty() {
                    while !data.is_empty() {
                        if sink.write(data[0], &ctx) {
                            data = &data[1..];
                            nwritten += 1;
                        } else {
                            break;
                        }
                    }
                }

                // Push whatever is left into the buffer
                while !inner.is_full() {
                    if let Some((c, rest)) = data.split_first() {
                        inner.buf.push_back(*c);
                        data = rest;
                        nwritten += 1;
                    } else {
                        break;
                    }
                }

                if nwritten > 0 {
                    return Some(nwritten);
                } else {
                    inner.wait_empty = true;
                }
            }

            self.notify.notified().await;
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

/// Copy available data from a Vec. Any remaining data will be copied to the
/// front of the Vec, truncating the vacated space without altering its
/// allocated capacity.
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

#[test]
fn test_copy_and_consume_1() {
    // Test copy_and_consume behaviour:
    // - source is copied to dest, and number of u8 copied is returned
    // - source is truncated without altering capacity

    let mut buf = vec![
        108, 111, 99, 97, 108, 104, 111, 115, 116, 58, 126, 35, 32, 27, 91, 54,
        110,
    ];
    let mut output = [0u8; 8];

    // before anything, assert len and capacity
    assert_eq!(buf.len(), 17);
    assert_eq!(buf.capacity(), 17);

    let n = copy_and_consume(&mut buf, &mut output[..]);

    // assert copy_and_consume fills output
    assert_eq!(n, 8);

    // assert capacity has not changed
    assert_eq!(buf.capacity(), 17);

    // assert copy_and_consume modify their arguments.
    assert_eq!(output[..n], vec![108, 111, 99, 97, 108, 104, 111, 115]);
    assert_eq!(buf, vec![116, 58, 126, 35, 32, 27, 91, 54, 110]);

    let n = copy_and_consume(&mut buf, &mut output[..]);

    // assert copy_and_consume fills output
    assert_eq!(n, 8);

    // assert capacity has not changed
    assert_eq!(buf.capacity(), 17);

    // assert further argument modification
    assert_eq!(output[..n], vec![116, 58, 126, 35, 32, 27, 91, 54]);
    assert_eq!(buf, vec![110]);

    let n = copy_and_consume(&mut buf, &mut output[..]);

    // assert copy_and_consume cannot fill output this time
    assert_eq!(n, 1);

    // assert capacity has not changed
    assert_eq!(buf.capacity(), 17);

    // assert further argument modification
    assert_eq!(output[..n], vec![110]);
    assert!(buf.is_empty());

    // assert that when copy_and_consume's source is empty, it does nothing
    let n = copy_and_consume(&mut buf, &mut output[..]);
    assert_eq!(n, 0);

    // assert that the output of copy_and_consume is consistent with it doing
    // nothing
    assert_eq!(buf.capacity(), 17);
    assert!(output[..n].is_empty());
    assert!(buf.is_empty());

    // assert that when it does nothing, output isn't changed
    assert_eq!(output[0..1], vec![110]);
}

#[test]
fn test_copy_and_consume_one_u8() {
    // Test that copy_and_consume works when source is one u8.
    let mut buf = vec![108];
    let mut output = [0u8; 8];

    assert_eq!(buf.len(), 1);
    assert_eq!(buf.capacity(), 1);

    let n = copy_and_consume(&mut buf, &mut output[..]);

    // only one u8 to read from source
    assert_eq!(n, 1);

    // assert that one u8 is read, that the source is now empty, and that
    // capacity is unchanged.
    assert_eq!(output[..n], vec![108]);
    assert!(buf.is_empty());
    assert_eq!(buf.capacity(), 1);
}

#[cfg(test)]
impl Params {
    pub(crate) fn test_defaults() -> Self {
        Self {
            poll_interval: Duration::from_millis(10),
            poll_miss_thresh: 2,
            buf_size: NonZeroUsize::new(16).unwrap(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::future::Future;
    use std::io::Result;

    use super::*;
    use crate::chardev::*;
    use crate::dispatch::DispCtx;
    use crate::instance::{Instance, ReqState};

    use futures::FutureExt;
    use tokio::runtime::Handle;

    async fn async_inst_test<T, F>(task: T) -> Result<()>
    where
        T: FnOnce(AsyncCtx) -> F,
        F: Future<Output = ()> + Send + 'static,
    {
        let inst = Instance::new_test(Some(Handle::current()))?;
        let _ = inst.set_target_state(ReqState::Run).unwrap();

        let ctx = inst.disp.async_ctx();
        task(ctx).await;

        Ok(())
    }

    #[tokio::test]
    async fn read_empty_returns_zero_bytes() {
        let _ = async_inst_test(|actx| async move {
            let uart = Arc::new(TestUart::new(4, 4));
            let rpoll = SourceBuffer::new(Params::test_defaults());
            rpoll.attach(uart.as_ref());

            let mut output = [];
            let res =
                rpoll.read(&mut output, uart.as_ref(), &actx).await.unwrap();
            assert_eq!(res, 0);
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn write_empty_fills_zero_bytes() {
        let _ = async_inst_test(|actx| async move {
            let uart = Arc::new(TestUart::new(4, 4));
            let wpoll = SinkBuffer::new(NonZeroUsize::new(16).unwrap());
            wpoll.attach(uart.as_ref());

            let input = [];
            let res = wpoll.write(&input, uart.as_ref(), &actx).await.unwrap();
            assert_eq!(res, 0);
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn read_byte() {
        let _ = async_inst_test(|actx| async move {
            let uart = Arc::new(TestUart::new(4, 4));
            let rpoll = SourceBuffer::new(Params::test_defaults());
            rpoll.attach(uart.as_ref());

            // If the guest writes a byte...
            uart.push_source(0xFE);
            uart.notify_source(&actx).await;

            let mut output = [0u8; 16];
            // ... We can read that byte.
            assert_eq!(
                1,
                rpoll.read(&mut output, uart.as_ref(), &actx).await.unwrap()
            );
            assert_eq!(output[0], 0xFE);
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn read_bytes() {
        let _ = async_inst_test(|actx| async move {
            let uart = Arc::new(TestUart::new(2, 2));
            let rpoll = SourceBuffer::new(Params::test_defaults());
            rpoll.attach(uart.as_ref());

            // If the guest writes multiple bytes...
            uart.push_source(0x0A);
            uart.push_source(0x0B);
            uart.notify_source(&actx).await;

            let mut output = [0u8; 16];
            // ... We can read them.
            assert_eq!(
                2,
                rpoll.read(&mut output, uart.as_ref(), &actx).await.unwrap()
            );
            assert_eq!(output[0], 0x0A);
            assert_eq!(output[1], 0x0B);
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn read_bytes_blocking() {
        let _ = async_inst_test(|actx| async move {
            let uart = Arc::new(TestUart::new(4, 4));
            let rpoll = SourceBuffer::new(Params::test_defaults());
            rpoll.attach(uart.as_ref());

            let mut output = [0u8; 16];

            // Before the source has been filled, reads should not succeed.
            futures::select! {
                _ = rpoll.read(&mut output, uart.as_ref(), &actx).fuse() => {
                    panic!("Shouldn't be readable")
                }
                default => {}
            }

            uart.push_source(0xFE);
            uart.notify_source(&actx).await;

            // However, once the uart has identified that it is readable, we can
            // begin reading bytes.
            assert_eq!(
                1,
                rpoll.read(&mut output, uart.as_ref(), &actx).await.unwrap()
            );
            assert_eq!(output[0], 0xFE);
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn write_byte() {
        let _ = async_inst_test(|actx| async move {
            let uart = Arc::new(TestUart::new(4, 4));
            let wpoll = SinkBuffer::new(NonZeroUsize::new(16).unwrap());
            wpoll.attach(uart.as_ref());

            let input = [0xFE];
            // If we write a byte...
            assert_eq!(
                1,
                wpoll.write(&input, uart.as_ref(), &actx).await.unwrap()
            );

            // ... The guest can read it.
            assert_eq!(uart.pop_sink().unwrap(), 0xFE);
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn write_bytes() {
        let _ = async_inst_test(|actx| async move {
            let uart = Arc::new(TestUart::new(4, 4));
            let wpoll = SinkBuffer::new(NonZeroUsize::new(16).unwrap());
            wpoll.attach(uart.as_ref());

            let input = [0x0A, 0x0B];
            // If we write multiple bytes...
            assert_eq!(
                2,
                wpoll.write(&input, uart.as_ref(), &actx).await.unwrap()
            );

            // ... The guest can read them.
            assert_eq!(uart.pop_sink().unwrap(), 0x0A);
            assert_eq!(uart.pop_sink().unwrap(), 0x0B);
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn write_bytes_beyond_internal_buffer_size() {
        let _ = async_inst_test(|actx| async move {
            let uart = Arc::new(TestUart::new(1, 1));
            let wpoll = SinkBuffer::new(NonZeroUsize::new(3).unwrap());
            wpoll.attach(uart.as_ref());
            assert_eq!(3, wpoll.inner.lock().unwrap().buf.capacity());

            // By attempting to write five bytes, we fill the following pipeline
            // in stages:
            //
            // [Client] -> [Serial Buffer] -> [UART]
            //             ^ 3 byte cap       ^ 1 byte cap
            //
            // After both the serial buffer and UART are saturated (four bytes
            // total) the write future will no longer complete successfully.
            //
            // Once this occurs, the UART will need to pop data from the
            // incoming sink to make space for subsequent writes.
            let input = [0x0A, 0x0B, 0x0C, 0x0D, 0x0E];
            assert_eq!(
                4,
                wpoll.write(&input, uart.as_ref(), &actx).await.unwrap()
            );

            futures::select! {
                _ = wpoll.write(&input[4..], uart.as_ref(), &actx).fuse() => {
                    panic!("Shouldn't be writable")
                }
                default => {}
            }

            assert_eq!(uart.pop_sink().unwrap(), 0x0A);
            uart.notify_sink(&actx).await;

            // After a byte is popped, the last byte becomes writable.
            assert_eq!(
                1,
                wpoll.write(&input[4..], uart.as_ref(), &actx).await.unwrap()
            );

            assert_eq!(uart.pop_sink().unwrap(), 0x0B);
            uart.notify_sink(&actx).await;
            assert_eq!(uart.pop_sink().unwrap(), 0x0C);
            uart.notify_sink(&actx).await;
            assert_eq!(uart.pop_sink().unwrap(), 0x0D);
            uart.notify_sink(&actx).await;
            assert_eq!(uart.pop_sink().unwrap(), 0x0E);
        })
        .await
        .unwrap();
    }

    struct TestUart {
        // The "capacity" fields here are a little redundant with the underlying
        // VecDeque capacities, but those values may get rounded up.
        //
        // To be more precise with "blocking-on-buffer-full" tests, we preserve
        // the original requested capacity value, which may be smaller.
        sink_cap: usize,
        sink: Mutex<VecDeque<u8>>,
        source_cap: usize,
        source: Mutex<VecDeque<u8>>,
        sink_notifier: NotifierCell<dyn Sink>,
        source_notifier: NotifierCell<dyn Source>,
        auto_discard: AtomicBool,
    }

    impl TestUart {
        fn new(sink_size: usize, source_size: usize) -> Self {
            TestUart {
                sink_cap: sink_size,
                sink: Mutex::new(VecDeque::with_capacity(sink_size)),
                source_cap: source_size,
                source: Mutex::new(VecDeque::with_capacity(source_size)),
                sink_notifier: NotifierCell::new(),
                source_notifier: NotifierCell::new(),
                auto_discard: AtomicBool::new(true),
            }
        }

        // Add a byte which can later get popped out of the source.
        fn push_source(&self, byte: u8) {
            let mut source = self.source.lock().unwrap();
            assert!(source.len() < self.source_cap);
            source.push_back(byte);
        }

        // Pop a byte out of the sink.
        fn pop_sink(&self) -> Option<u8> {
            let mut sink = self.sink.lock().unwrap();
            sink.pop_front()
        }

        async fn notify_source(&self, actx: &AsyncCtx) {
            let ctx = actx.dispctx().await.unwrap();
            self.source_notifier.notify(self, &ctx);
        }

        async fn notify_sink(&self, actx: &AsyncCtx) {
            let ctx = actx.dispctx().await.unwrap();
            self.sink_notifier.notify(self, &ctx);
        }
    }

    impl Sink for TestUart {
        fn write(&self, data: u8, _ctx: &DispCtx) -> bool {
            let mut sink = self.sink.lock().unwrap();
            if sink.len() < self.sink_cap {
                sink.push_back(data);
                true
            } else {
                false
            }
        }
        fn set_notifier(&self, f: Option<SinkNotifier>) {
            self.sink_notifier.set(f);
        }
    }

    impl Source for TestUart {
        fn read(&self, _ctx: &DispCtx) -> Option<u8> {
            let mut source = self.source.lock().unwrap();
            source.pop_front()
        }
        fn discard(&self, _count: usize, _ctx: &DispCtx) -> usize {
            panic!();
        }
        fn set_autodiscard(&self, active: bool) {
            self.auto_discard.store(active, Ordering::SeqCst);
        }
        fn set_notifier(&self, f: Option<SourceNotifier>) {
            self.source_notifier.set(f);
        }
    }
}
