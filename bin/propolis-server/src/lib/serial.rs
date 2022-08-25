//! Routines to expose a connection to an instance's serial port.

use std::num::NonZeroUsize;
use std::ops::Range;
use std::sync::Arc;
use std::time::Duration;

use futures::future::Fuse;
use futures::stream::SplitSink;
use futures::{FutureExt, SinkExt, StreamExt};
use hyper::upgrade::Upgraded;
use propolis::chardev::{pollers, Sink, Source};
use propolis::hw::uart::LpcUart;
use slog::{info, Logger};
use thiserror::Error;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{tungstenite, WebSocketStream};

/// Errors which may occur during the course of a serial connection.
#[derive(Error, Debug)]
pub enum SerialTaskError {
    #[error("Cannot upgrade HTTP request to WebSockets: {0}")]
    Upgrade(#[from] hyper::Error),

    #[error("WebSocket Error: {0}")]
    WebSocket(#[from] tungstenite::Error),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

pub struct SerialTask {
    /// Handle to attached serial session
    pub task: JoinHandle<()>,
    /// Oneshot channel used to signal the task to terminate gracefully
    pub close_ch: Option<oneshot::Sender<()>>,
    /// Channel used to send new client connections to the streaming task
    pub websocks_ch: mpsc::Sender<WebSocketStream<Upgraded>>,
}

impl Drop for SerialTask {
    fn drop(&mut self) {
        if let Some(ch) = self.close_ch.take() {
            let _ = ch.send(());
        } else {
            self.task.abort();
        }
    }
}

pub async fn instance_serial_task(
    mut websocks_recv: mpsc::Receiver<WebSocketStream<Upgraded>>,
    mut close_recv: oneshot::Receiver<()>,
    serial: Arc<Serial<LpcUart>>,
    log: Logger,
) -> Result<(), SerialTaskError> {
    let mut output = [0u8; 1024];
    let mut cur_output: Option<Range<usize>> = None;
    let mut cur_input: Option<(Vec<u8>, usize)> = None;

    let mut ws_sinks: Vec<SplitSink<WebSocketStream<Upgraded>, Message>> =
        Vec::new();
    let mut ws_streams: Vec<
        futures::stream::SplitStream<WebSocketStream<Upgraded>>,
    > = Vec::new();

    let (send_ch, mut recv_ch) = mpsc::channel(4);

    loop {
        let (uart_read, ws_send) =
            match &cur_output {
                None => {
                    (serial.read_source(&mut output).fuse(), Fuse::terminated())
                }
                Some(r) => (
                    Fuse::terminated(),
                    if !ws_sinks.is_empty() {
                        futures::stream::iter(ws_sinks.iter_mut().zip(
                            std::iter::repeat(Vec::from(&output[r.clone()])),
                        ))
                        .for_each_concurrent(4, |(ws, bin)| {
                            ws.send(Message::binary(bin)).map(|_| ())
                        })
                        .fuse()
                    } else {
                        Fuse::terminated()
                    },
                ),
            };

        let (ws_recv, uart_write) = match &cur_input {
            None => (
                if !ws_streams.is_empty() {
                    futures::stream::iter(ws_streams.iter_mut().enumerate())
                        .for_each_concurrent(4, |(i, ws)| {
                            // if we don't `move` below, rustc says that `i`
                            // (which is usize: Copy (!)) is borrowed. but if we
                            // move without making this explicit reference here,
                            // it moves send_ch into the closure.
                            let ch = &send_ch;
                            ws.next()
                                .then(move |msg| ch.send((i, msg)))
                                .map(|_| ())
                        })
                        .fuse()
                } else {
                    Fuse::terminated()
                },
                Fuse::terminated(),
            ),
            Some((data, consumed)) => (
                Fuse::terminated(),
                serial.write_sink(&data[*consumed..]).fuse(),
            ),
        };

        let recv_ch_fut = recv_ch.recv().fuse();

        tokio::select! {
            // Poll in the order written
            biased;

            // It's important we always poll the close channel first
            // so that a constant stream of incoming/outgoing messages
            // don't cause us to ignore it
            _ = &mut close_recv => {
                info!(log, "Terminating serial task");
                break;
            }

            new_ws = websocks_recv.recv() => {
                if let Some(ws) = new_ws {
                    let (ws_sink, ws_stream) = ws.split();
                    ws_sinks.push(ws_sink);
                    ws_streams.push(ws_stream);
                }
            }

            // Write bytes into the UART from the WS
            written = uart_write => {
                match written {
                    Some(0) | None => break,
                    Some(n) => {
                        let (data, consumed) = cur_input.as_mut().unwrap();
                        *consumed += n;
                        if *consumed == data.len() {
                            cur_input = None;
                        }
                    }
                }
            }

            // Transmit bytes from the UART through the WS
            _ = ws_send => {
                cur_output = None;
            }

            // Read bytes from the UART to be transmitted out the WS
            nread = uart_read => {
                match nread {
                    Some(0) | None => break,
                    Some(n) => { cur_output = Some(0..n) }
                }
            }

            // Receive bytes from the intermediate channel to be injected into
            // the UART. This needs to be checked before `ws_recv` so that
            // "close" messages can be processed and their indicated
            // sinks/streams removed before they are polled again.
            pair = recv_ch_fut => {
                if let Some((i, msg)) = pair {
                    match msg {
                        Some(Ok(Message::Binary(input))) => {
                            cur_input = Some((input, 0));
                        }
                        Some(Ok(Message::Close(..))) | None => {
                            info!(log, "Removed a closed serial connection.");
                            let _ = ws_sinks.remove(i).close().await;
                            let _ = ws_streams.remove(i);
                        },
                        _ => continue,
                    }
                }
            }

            // Receive bytes from connected WS clients to feed to the
            // intermediate recv_ch
            _ = ws_recv => {}
        }
    }
    Ok(())
}

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

    pub async fn read_source(&self, buf: &mut [u8]) -> Option<usize> {
        self.source_poller.read(buf, self.uart.as_ref()).await
    }

    pub async fn write_sink(&self, buf: &[u8]) -> Option<usize> {
        self.sink_poller.write(buf, self.uart.as_ref()).await
    }
}

impl<Device: Sink + Source> Drop for Serial<Device> {
    fn drop(&mut self) {
        self.uart.set_autodiscard(true);
    }
}
