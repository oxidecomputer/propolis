use std::collections::VecDeque;
use std::io::Result;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

use propolis::chardev::{Sink, Source};

// Supports two functions:
// - Moving bytes from a client into a temporary buffer
// - Moving bytes from that temporary buffer to the underlying device sink
struct SinkDriver<Ctx> {
    sink: Arc<dyn Sink<Ctx>>,
    buf: VecDeque<u8>,
    waker: Option<Waker>,
}

impl<Ctx: 'static> SinkDriver<Ctx> {
    // Writes to the internal buffer with as much of `buf` as possible.
    // Returns the number of bytes written.
    fn write_to_buffer(&mut self, buf: &[u8]) -> usize {
        let to_fill =
            std::cmp::min(buf.len(), self.buf.capacity() - self.buf.len());
        self.buf.extend(buf[..to_fill].iter());
        to_fill
    }

    fn buffer_to_sink(&mut self) {
        let mut wrote_bytes = false;
        while let Some(b) = self.buf.pop_front() {
            if !self.sink.sink_write(b) {
                self.buf.push_front(b);
                break;
            }
            wrote_bytes = true;
        }
        if wrote_bytes {
            if let Some(w) = self.waker.take() {
                // We made room; wake the writing client if one exists.
                w.wake();
            }
        }
    }
}

// Supports two functions:
// - Moving bytes from a temporary buffer out to a client
// - Moving bytes from the underlying device source into that temporary buffer
struct SourceDriver<Ctx> {
    source: Arc<dyn Source<Ctx>>,
    buf: VecDeque<u8>,
    waker: Option<Waker>,
}

impl<Ctx: 'static> SourceDriver<Ctx> {
    // Reads from the internal buffer to `buf`.
    // Returns the number of bytes read.
    fn read_from_buffer(&mut self, buf: &mut ReadBuf<'_>) -> usize {
        // Since the source buffer may not be contiguous, this is performed in
        // two phases (operating on up to two slices).
        let (first, second) = self.buf.as_slices();
        let first_fill = std::cmp::min(buf.remaining(), first.len());
        if first_fill == 0 {
            return 0;
        }
        buf.put_slice(&first[..first_fill]);
        let second_fill = std::cmp::min(buf.remaining(), second.len());
        if second_fill > 0 {
            buf.put_slice(&second[..second_fill]);
        }
        self.buf.drain(..first_fill + second_fill);
        return first_fill + second_fill;
    }

    fn source_to_buffer(&mut self) {
        let mut wrote_bytes = false;
        while self.buf.len() != self.buf.capacity() {
            if let Some(b) = self.source.source_read() {
                self.buf.push_back(b);
                wrote_bytes = true;
            } else {
                break;
            }
        }
        if wrote_bytes {
            if let Some(waker) = self.waker.take() {
                waker.wake();
            }
        }
    }
}

/// Represents a serial connection into the VM.
pub struct Serial<Ctx, Device: Sink<Ctx> + Source<Ctx>> {
    uart: Arc<Device>,

    sink_driver: Arc<Mutex<SinkDriver<Ctx>>>,
    source_driver: Arc<Mutex<SourceDriver<Ctx>>>,
}

impl<Ctx: 'static, Device: Sink<Ctx> + Source<Ctx>> Serial<Ctx, Device> {
    /// Creates a new buffered serial connection on top of `uart.`
    ///
    /// The primary interfaces for interacting with this object
    /// is [`tokio::io::AsyncRead`] for reading, and [`tokio::io::AsyncWrite`]
    /// for writing.
    ///
    /// Creation of this object disables "autodiscard", and destruction
    /// of the object re-enables "autodiscard" mode.
    ///
    /// # Arguments
    ///
    /// * `uart` - The device which data will be read from / written to.
    /// * `sink_size` - A lower bound on the size of the writeback buffer.
    /// * `source_size` - A lower bound on the size of the read buffer.
    pub fn new(
        uart: Arc<Device>,
        sink_size: usize,
        source_size: usize,
    ) -> Serial<Ctx, Device> {
        let sink = Arc::clone(&uart) as Arc<dyn Sink<Ctx>>;
        let sink_driver = Arc::new(Mutex::new(SinkDriver {
            sink: sink.clone(),
            buf: VecDeque::with_capacity(sink_size),
            waker: None,
        }));

        let notifier_sink = sink_driver.clone();
        sink.sink_set_notifier(Box::new(move |_| {
            let mut sink_driver = notifier_sink.lock().unwrap();
            sink_driver.buffer_to_sink();
        }));

        let source = Arc::clone(&uart) as Arc<dyn Source<Ctx>>;
        let source_driver = Arc::new(Mutex::new(SourceDriver {
            source: source.clone(),
            buf: VecDeque::with_capacity(source_size),
            waker: None,
        }));

        let notifier_source = source_driver.clone();
        source.source_set_notifier(Box::new(move |_| {
            let mut source_driver = notifier_source.lock().unwrap();
            source_driver.source_to_buffer();
        }));

        uart.source_set_autodiscard(false);
        Serial { uart, sink_driver, source_driver }
    }
}

impl<Ctx, Device: Sink<Ctx> + Source<Ctx>> Drop for Serial<Ctx, Device> {
    fn drop(&mut self) {
        self.uart.source_set_autodiscard(true);
    }
}

impl<Ctx: 'static, Device: Sink<Ctx> + Source<Ctx>> AsyncWrite
    for Serial<Ctx, Device>
{
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize>> {
        let mut sink = self.get_mut().sink_driver.lock().unwrap();
        if buf.len() == 0 {
            return Poll::Ready(Ok(0));
        }
        let n = sink.write_to_buffer(&buf);
        match n {
            0 => {
                sink.waker = Some(cx.waker().clone());
                Poll::Pending
            }
            n => {
                // After freeing up any space in the temporary buffer, access
                // the underlying device - doing so here makes up for any missed
                // notifications that may have occurred.
                sink.buffer_to_sink();
                Poll::Ready(Ok(n))
            }
        }
    }

    fn poll_flush(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
    ) -> Poll<Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
    ) -> Poll<Result<()>> {
        Poll::Ready(Ok(()))
    }
}

impl<Ctx: 'static, Device: Sink<Ctx> + Source<Ctx>> AsyncRead
    for Serial<Ctx, Device>
{
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<Result<()>> {
        if buf.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }
        let mut source = self.get_mut().source_driver.lock().unwrap();
        if source.read_from_buffer(buf) == 0 {
            source.waker = Some(cx.waker().clone());
            return Poll::Pending;
        }
        // After freeing up any space in the temporary buffer, access the
        // underlying device - doing so here makes up for any missed
        // notifications that may have occurred.
        source.source_to_buffer();
        Poll::Ready(Ok(()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::FutureExt;
    use propolis::chardev::Notifier;
    use std::sync::atomic::{AtomicBool, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

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
        sink_notifier: Mutex<Option<Notifier<()>>>,
        source_notifier: Mutex<Option<Notifier<()>>>,
        auto_discard: AtomicBool,
    }

    impl TestUart {
        fn new(sink_size: usize, source_size: usize) -> Self {
            TestUart {
                sink_cap: sink_size,
                sink: Mutex::new(VecDeque::with_capacity(sink_size)),
                source_cap: source_size,
                source: Mutex::new(VecDeque::with_capacity(source_size)),
                sink_notifier: Mutex::new(None),
                source_notifier: Mutex::new(None),
                auto_discard: AtomicBool::new(true),
            }
        }

        // Add a byte which can later get popped out of the source.
        fn push_source(&self, byte: u8) {
            let mut source = self.source.lock().unwrap();
            assert!(source.len() < self.source_cap);
            source.push_back(byte);
        }
        fn notify_source(&self) {
            let source_notifier = self.source_notifier.lock().unwrap();
            if let Some(n) = source_notifier.as_ref() {
                n(&());
            }
        }

        // Pop a byte out of the sink.
        fn pop_sink(&self) -> Option<u8> {
            let mut sink = self.sink.lock().unwrap();
            sink.pop_front()
        }
        fn notify_sink(&self) {
            let sink_notifier = self.sink_notifier.lock().unwrap();
            if let Some(n) = sink_notifier.as_ref() {
                n(&());
            }
        }
    }

    impl Sink<()> for TestUart {
        fn sink_write(&self, data: u8) -> bool {
            let mut sink = self.sink.lock().unwrap();
            if sink.len() < self.sink_cap {
                sink.push_back(data);
                true
            } else {
                false
            }
        }
        fn sink_set_notifier(&self, f: Notifier<()>) {
            let mut sink_notifier = self.sink_notifier.lock().unwrap();
            assert!(sink_notifier.is_none());
            *sink_notifier = Some(f);
        }
    }

    impl Source<()> for TestUart {
        fn source_read(&self) -> Option<u8> {
            let mut source = self.source.lock().unwrap();
            source.pop_front()
        }
        fn source_discard(&self, _count: usize) -> usize {
            panic!();
        }
        fn source_set_autodiscard(&self, active: bool) {
            self.auto_discard.store(active, Ordering::SeqCst);
        }
        fn source_set_notifier(&self, f: Notifier<()>) {
            let mut source_notifier = self.source_notifier.lock().unwrap();
            assert!(source_notifier.is_none());
            *source_notifier = Some(f);
        }
    }

    #[tokio::test]
    async fn serial_turns_off_autodiscard() {
        let uart = Arc::new(TestUart::new(4, 4));
        // Auto-discard is "on" before the serial object, "off" when the object
        // is alive, and back "on" after the object has been dropped.
        assert!(uart.auto_discard.load(Ordering::SeqCst));

        let serial = Serial::new(uart.clone(), 16, 16);
        assert!(!uart.auto_discard.load(Ordering::SeqCst));

        drop(serial);
        assert!(uart.auto_discard.load(Ordering::SeqCst));
    }
    #[tokio::test]
    async fn read_empty_returns_zero_bytes() {
        let uart = Arc::new(TestUart::new(4, 4));
        let mut serial = Serial::new(uart.clone(), 16, 16);

        let mut output = [];
        assert_eq!(0, serial.read(&mut output).await.unwrap());
    }

    #[tokio::test]
    async fn write_empty_fills_zero_bytes() {
        let uart = Arc::new(TestUart::new(4, 4));
        let mut serial = Serial::new(uart.clone(), 16, 16);

        let input = [];
        assert_eq!(0, serial.write(&input).await.unwrap());
    }

    #[tokio::test]
    async fn read_byte() {
        let uart = Arc::new(TestUart::new(4, 4));
        let mut serial = Serial::new(uart.clone(), 16, 16);

        // If the guest writes a byte...
        uart.push_source(0xFE);
        uart.notify_source();

        let mut output = [0u8; 16];
        // ... We can read that byte.
        assert_eq!(1, serial.read(&mut output).await.unwrap());
        assert_eq!(output[0], 0xFE);
    }

    #[tokio::test]
    async fn read_bytes() {
        let uart = Arc::new(TestUart::new(2, 2));
        let mut serial = Serial::new(uart.clone(), 16, 16);

        // If the guest writes multiple bytes...
        uart.push_source(0x0A);
        uart.push_source(0x0B);
        uart.notify_source();

        let mut output = [0u8; 16];
        // ... We can read them.
        assert_eq!(2, serial.read(&mut output).await.unwrap());
        assert_eq!(output[0], 0x0A);
        assert_eq!(output[1], 0x0B);
    }

    #[tokio::test]
    async fn read_bytes_beyond_internal_buffer_size() {
        let uart = Arc::new(TestUart::new(4, 4));
        let mut serial = Serial::new(uart.clone(), 3, 3);
        assert_eq!(3, serial.source_driver.lock().unwrap().buf.capacity());

        // We write four bytes, yet trigger the notification mechanism once.
        //
        // The Serial buffer size (3) is smaller than the Uart's buffer (4).
        uart.push_source(0x0A);
        uart.push_source(0x0B);
        uart.push_source(0x0C);
        uart.push_source(0x0D);
        uart.notify_source();

        // We are still able to read the subsequent bytes (without blocking),
        // just in two batches.
        let mut output = [0u8; 16];
        assert_eq!(3, serial.read(&mut output).await.unwrap());
        assert_eq!(output[0], 0x0A);
        assert_eq!(output[1], 0x0B);
        assert_eq!(output[2], 0x0C);
        assert_eq!(1, serial.read(&mut output).await.unwrap());
        assert_eq!(output[0], 0x0D);
    }

    #[tokio::test]
    async fn read_bytes_blocking() {
        let uart = Arc::new(TestUart::new(4, 4));
        let mut serial = Serial::new(uart.clone(), 3, 3);
        assert_eq!(3, serial.source_driver.lock().unwrap().buf.capacity());

        let mut output = [0u8; 16];

        // Before the source has been filled, reads should not succeed.
        futures::select! {
            _ = serial.read(&mut output).fuse() => panic!("Shouldn't be readable"),
            default => {}
        }

        uart.push_source(0xFE);

        // Note that even with bytes in the source, without a notification,
        // they aren't readable.
        futures::select! {
            _ = serial.read(&mut output).fuse() => panic!("Shouldn't be readable"),
            default => {}
        }

        // However, once the uart has identified that it is readable, we can
        // begin reading bytes.
        uart.notify_source();
        assert_eq!(1, serial.read(&mut output).await.unwrap());
        assert_eq!(output[0], 0xFE);
    }

    #[tokio::test]
    async fn write_byte() {
        let uart = Arc::new(TestUart::new(4, 4));
        let mut serial = Serial::new(uart.clone(), 16, 16);

        let input = [0xFE];
        // If we write a byte...
        assert_eq!(1, serial.write(&input).await.unwrap());

        // ... The guest can read it.
        assert_eq!(uart.pop_sink().unwrap(), 0xFE);
    }

    #[tokio::test]
    async fn write_bytes() {
        let uart = Arc::new(TestUart::new(4, 4));
        let mut serial = Serial::new(uart.clone(), 16, 16);

        let input = [0x0A, 0x0B];
        // If we write multiple bytes...
        assert_eq!(2, serial.write(&input).await.unwrap());

        // ... The guest can read them.
        assert_eq!(uart.pop_sink().unwrap(), 0x0A);
        assert_eq!(uart.pop_sink().unwrap(), 0x0B);
    }

    #[tokio::test]
    async fn write_bytes_beyond_internal_buffer_size() {
        let uart = Arc::new(TestUart::new(1, 1));
        let mut serial = Serial::new(uart.clone(), 3, 3);
        assert_eq!(3, serial.sink_driver.lock().unwrap().buf.capacity());

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
        assert_eq!(3, serial.write(&input).await.unwrap());
        assert_eq!(1, serial.write(&input[3..]).await.unwrap());

        futures::select! {
            _ = serial.write(&input[4..]).fuse() => panic!("Shouldn't be writable"),
            default => {}
        }

        assert_eq!(uart.pop_sink().unwrap(), 0x0A);
        uart.notify_sink();

        // After a byte is popped, the last byte becomes writable.
        assert_eq!(1, serial.write(&input[4..]).await.unwrap());

        assert_eq!(uart.pop_sink().unwrap(), 0x0B);
        uart.notify_sink();
        assert_eq!(uart.pop_sink().unwrap(), 0x0C);
        uart.notify_sink();
        assert_eq!(uart.pop_sink().unwrap(), 0x0D);
        uart.notify_sink();
        assert_eq!(uart.pop_sink().unwrap(), 0x0E);
    }
}
