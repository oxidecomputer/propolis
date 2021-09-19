use std::sync::Arc;
use std::num::NonZeroUsize;
use std::time::Duration;

use propolis::chardev::{Sink, Source, pollers};
use propolis::dispatch::AsyncCtx;

/// Represents a serial connection into the VM.
pub struct Serial<Device: Sink + Source> {
    uart: Arc<Device>,

    sink_poller: Arc<pollers::SinkBuffer>,
    source_poller: Arc<pollers::SourceBuffer>,
}

impl<Device: Sink + Source> Serial<Device> {
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
        sink_size: NonZeroUsize,
        source_size: NonZeroUsize,
    ) -> Serial<Device> {
        let sink_poller = pollers::SinkBuffer::new(sink_size);
        let source_poller = pollers::SourceBuffer::new(pollers::Params {
            buf_size: source_size,
            poll_interval: Duration::from_millis(10),
            poll_miss_thresh: 5,
        });
        sink_poller.attach(uart.as_ref());
        source_poller.attach(uart.as_ref());
        uart.set_autodiscard(false);

        Serial { uart, sink_poller, source_poller }
    }

    pub async fn read_source(&self, buf: &mut [u8], actx: &AsyncCtx) -> Option<usize> {
        self.source_poller.read(buf, self.uart.as_ref(), actx).await
    }

    pub async fn write_sink(&self, buf: &[u8], actx: &AsyncCtx) -> Option<usize> {
        self.sink_poller.write(buf, self.uart.as_ref(), actx).await
    }
}

impl<Device: Sink + Source> Drop for Serial<Device> {
    fn drop(&mut self) {
        self.uart.set_autodiscard(true);
    }
}


//#[cfg(test)]
//mod tests {
//    use super::*;
//    use futures::FutureExt;
//    use propolis::chardev::{SinkNotifier, SourceNotifier};
//    use std::sync::atomic::{AtomicBool, Ordering};
//    use tokio::io::{AsyncReadExt, AsyncWriteExt};

//    struct TestUart {
//        // The "capacity" fields here are a little redundant with the underlying
//        // VecDeque capacities, but those values may get rounded up.
//        //
//        // To be more precise with "blocking-on-buffer-full" tests, we preserve
//        // the original requested capacity value, which may be smaller.
//        sink_cap: usize,
//        sink: Mutex<VecDeque<u8>>,
//        source_cap: usize,
//        source: Mutex<VecDeque<u8>>,
//        sink_notifier: Mutex<Option<SinkNotifier>>,
//        source_notifier: Mutex<Option<SourceNotifier>>,
//        auto_discard: AtomicBool,
//    }

//    impl TestUart {
//        fn new(sink_size: usize, source_size: usize) -> Self {
//            TestUart {
//                sink_cap: sink_size,
//                sink: Mutex::new(VecDeque::with_capacity(sink_size)),
//                source_cap: source_size,
//                source: Mutex::new(VecDeque::with_capacity(source_size)),
//                sink_notifier: Mutex::new(None),
//                source_notifier: Mutex::new(None),
//                auto_discard: AtomicBool::new(true),
//            }
//        }

//        // Add a byte which can later get popped out of the source.
//        fn push_source(&self, byte: u8) {
//            let mut source = self.source.lock().unwrap();
//            assert!(source.len() < self.source_cap);
//            source.push_back(byte);
//        }
//        fn notify_source(&self) {
//            let source_notifier = self.source_notifier.lock().unwrap();
//            if let Some(n) = source_notifier.as_ref() {
//                n(self as &dyn Source);
//            }
//        }

//        // Pop a byte out of the sink.
//        fn pop_sink(&self) -> Option<u8> {
//            let mut sink = self.sink.lock().unwrap();
//            sink.pop_front()
//        }
//        fn notify_sink(&self) {
//            let sink_notifier = self.sink_notifier.lock().unwrap();
//            if let Some(n) = sink_notifier.as_ref() {
//                n(self as &dyn Sink);
//            }
//        }
//    }

//    impl Sink for TestUart {
//        fn write(&self, data: u8) -> bool {
//            let mut sink = self.sink.lock().unwrap();
//            if sink.len() < self.sink_cap {
//                sink.push_back(data);
//                true
//            } else {
//                false
//            }
//        }
//        fn set_notifier(&self, f: SinkNotifier) {
//            let mut sink_notifier = self.sink_notifier.lock().unwrap();
//            assert!(sink_notifier.is_none());
//            *sink_notifier = Some(f);
//        }
//    }

//    impl Source for TestUart {
//        fn read(&self) -> Option<u8> {
//            let mut source = self.source.lock().unwrap();
//            source.pop_front()
//        }
//        fn discard(&self, _count: usize) -> usize {
//            panic!();
//        }
//        fn set_autodiscard(&self, active: bool) {
//            self.auto_discard.store(active, Ordering::SeqCst);
//        }
//        fn set_notifier(&self, f: SourceNotifier) {
//            let mut source_notifier = self.source_notifier.lock().unwrap();
//            assert!(source_notifier.is_none());
//            *source_notifier = Some(f);
//        }
//    }

//    #[tokio::test]
//    async fn serial_turns_off_autodiscard() {
//        let uart = Arc::new(TestUart::new(4, 4));
//        // Auto-discard is "on" before the serial object, "off" when the object
//        // is alive, and back "on" after the object has been dropped.
//        assert!(uart.auto_discard.load(Ordering::SeqCst));

//        let serial = Serial::new(uart.clone(), 16, 16);
//        assert!(!uart.auto_discard.load(Ordering::SeqCst));

//        drop(serial);
//        assert!(uart.auto_discard.load(Ordering::SeqCst));
//    }
//    #[tokio::test]
//    async fn read_empty_returns_zero_bytes() {
//        let uart = Arc::new(TestUart::new(4, 4));
//        let mut serial = Serial::new(uart.clone(), 16, 16);

//        let mut output = [];
//        assert_eq!(0, serial.read(&mut output).await.unwrap());
//    }

//    #[tokio::test]
//    async fn write_empty_fills_zero_bytes() {
//        let uart = Arc::new(TestUart::new(4, 4));
//        let mut serial = Serial::new(uart.clone(), 16, 16);

//        let input = [];
//        assert_eq!(0, serial.write(&input).await.unwrap());
//    }

//    #[tokio::test]
//    async fn read_byte() {
//        let uart = Arc::new(TestUart::new(4, 4));
//        let mut serial = Serial::new(uart.clone(), 16, 16);

//        // If the guest writes a byte...
//        uart.push_source(0xFE);
//        uart.notify_source();

//        let mut output = [0u8; 16];
//        // ... We can read that byte.
//        assert_eq!(1, serial.read(&mut output).await.unwrap());
//        assert_eq!(output[0], 0xFE);
//    }

//    #[tokio::test]
//    async fn read_bytes() {
//        let uart = Arc::new(TestUart::new(2, 2));
//        let mut serial = Serial::new(uart.clone(), 16, 16);

//        // If the guest writes multiple bytes...
//        uart.push_source(0x0A);
//        uart.push_source(0x0B);
//        uart.notify_source();

//        let mut output = [0u8; 16];
//        // ... We can read them.
//        assert_eq!(2, serial.read(&mut output).await.unwrap());
//        assert_eq!(output[0], 0x0A);
//        assert_eq!(output[1], 0x0B);
//    }

//    #[tokio::test]
//    async fn full_read_buffer_triggers_full_signal() {
//        let uart = Arc::new(TestUart::new(4, 4));
//        let mut serial = Serial::new(uart.clone(), 3, 3);
//        assert_eq!(3, serial.source_driver.lock().unwrap().buf.capacity());

//        let buffer_full_signal = |serial: &mut Serial<TestUart>| {
//            futures::select! {
//                _ = serial.read_buffer_full().fuse() => true,
//                default => false,
//            }
//        };

//        // The "buffer full signal" does not fire while we're filling up the
//        // buffer...
//        assert!(!buffer_full_signal(&mut serial));
//        uart.push_source(0x0A);
//        uart.notify_source();
//        assert!(!buffer_full_signal(&mut serial));

//        uart.push_source(0x0B);
//        uart.notify_source();
//        assert!(!buffer_full_signal(&mut serial));

//        // ... but as soon as the last byte has been written, the signal
//        // asserts.
//        uart.push_source(0x0C);
//        uart.notify_source();
//        assert!(buffer_full_signal(&mut serial));

//        let mut output = [0u8; 16];
//        assert_eq!(3, serial.read(&mut output).await.unwrap());

//        // The signal clears as soon as the read completes.
//        assert!(!buffer_full_signal(&mut serial));
//    }

//    #[tokio::test]
//    async fn read_bytes_beyond_internal_buffer_size() {
//        let uart = Arc::new(TestUart::new(4, 4));
//        let mut serial = Serial::new(uart.clone(), 3, 3);
//        assert_eq!(3, serial.source_driver.lock().unwrap().buf.capacity());

//        // We write four bytes, yet trigger the notification mechanism once.
//        //
//        // The Serial buffer size (3) is smaller than the Uart's buffer (4).
//        uart.push_source(0x0A);
//        uart.push_source(0x0B);
//        uart.push_source(0x0C);
//        uart.push_source(0x0D);
//        uart.notify_source();

//        // We are still able to read the subsequent bytes (without blocking),
//        // just in two batches.
//        let mut output = [0u8; 16];
//        assert_eq!(3, serial.read(&mut output).await.unwrap());
//        assert_eq!(output[0], 0x0A);
//        assert_eq!(output[1], 0x0B);
//        assert_eq!(output[2], 0x0C);
//        assert_eq!(1, serial.read(&mut output[3..]).await.unwrap());
//        assert_eq!(output[3], 0x0D);
//    }

//    #[tokio::test]
//    async fn read_bytes_blocking() {
//        let uart = Arc::new(TestUart::new(4, 4));
//        let mut serial = Serial::new(uart.clone(), 3, 3);
//        assert_eq!(3, serial.source_driver.lock().unwrap().buf.capacity());

//        let mut output = [0u8; 16];

//        // Before the source has been filled, reads should not succeed.
//        futures::select! {
//            _ = serial.read(&mut output).fuse() => panic!("Shouldn't be readable"),
//            default => {}
//        }

//        uart.push_source(0xFE);

//        // Note that even with bytes in the source, without a notification,
//        // they aren't readable.
//        futures::select! {
//            _ = serial.read(&mut output).fuse() => panic!("Shouldn't be readable"),
//            default => {}
//        }

//        // However, once the uart has identified that it is readable, we can
//        // begin reading bytes.
//        uart.notify_source();
//        assert_eq!(1, serial.read(&mut output).await.unwrap());
//        assert_eq!(output[0], 0xFE);
//    }

//    #[tokio::test]
//    async fn write_byte() {
//        let uart = Arc::new(TestUart::new(4, 4));
//        let mut serial = Serial::new(uart.clone(), 16, 16);

//        let input = [0xFE];
//        // If we write a byte...
//        assert_eq!(1, serial.write(&input).await.unwrap());

//        // ... The guest can read it.
//        assert_eq!(uart.pop_sink().unwrap(), 0xFE);
//    }

//    #[tokio::test]
//    async fn write_bytes() {
//        let uart = Arc::new(TestUart::new(4, 4));
//        let mut serial = Serial::new(uart.clone(), 16, 16);

//        let input = [0x0A, 0x0B];
//        // If we write multiple bytes...
//        assert_eq!(2, serial.write(&input).await.unwrap());

//        // ... The guest can read them.
//        assert_eq!(uart.pop_sink().unwrap(), 0x0A);
//        assert_eq!(uart.pop_sink().unwrap(), 0x0B);
//    }

//    #[tokio::test]
//    async fn write_bytes_beyond_internal_buffer_size() {
//        let uart = Arc::new(TestUart::new(1, 1));
//        let mut serial = Serial::new(uart.clone(), 3, 3);
//        assert_eq!(3, serial.sink_driver.lock().unwrap().buf.capacity());

//        // By attempting to write five bytes, we fill the following pipeline
//        // in stages:
//        //
//        // [Client] -> [Serial Buffer] -> [UART]
//        //             ^ 3 byte cap       ^ 1 byte cap
//        //
//        // After both the serial buffer and UART are saturated (four bytes
//        // total) the write future will no longer complete successfully.
//        //
//        // Once this occurs, the UART will need to pop data from the
//        // incoming sink to make space for subsequent writes.
//        let input = [0x0A, 0x0B, 0x0C, 0x0D, 0x0E];
//        assert_eq!(3, serial.write(&input).await.unwrap());
//        assert_eq!(1, serial.write(&input[3..]).await.unwrap());

//        futures::select! {
//            _ = serial.write(&input[4..]).fuse() => panic!("Shouldn't be writable"),
//            default => {}
//        }

//        assert_eq!(uart.pop_sink().unwrap(), 0x0A);
//        uart.notify_sink();

//        // After a byte is popped, the last byte becomes writable.
//        assert_eq!(1, serial.write(&input[4..]).await.unwrap());

//        assert_eq!(uart.pop_sink().unwrap(), 0x0B);
//        uart.notify_sink();
//        assert_eq!(uart.pop_sink().unwrap(), 0x0C);
//        uart.notify_sink();
//        assert_eq!(uart.pop_sink().unwrap(), 0x0D);
//        uart.notify_sink();
//        assert_eq!(uart.pop_sink().unwrap(), 0x0E);
//    }
//}
