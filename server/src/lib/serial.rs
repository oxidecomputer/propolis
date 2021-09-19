use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::Duration;

use propolis::chardev::{pollers, Sink, Source};
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

    pub async fn read_source(
        &self,
        buf: &mut [u8],
        actx: &AsyncCtx,
    ) -> Option<usize> {
        self.source_poller.read(buf, self.uart.as_ref(), actx).await
    }

    pub async fn write_sink(
        &self,
        buf: &[u8],
        actx: &AsyncCtx,
    ) -> Option<usize> {
        self.sink_poller.write(buf, self.uart.as_ref(), actx).await
    }
}

impl<Device: Sink + Source> Drop for Serial<Device> {
    fn drop(&mut self) {
        self.uart.set_autodiscard(true);
    }
}
