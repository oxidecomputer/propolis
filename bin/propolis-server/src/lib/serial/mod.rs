// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Routines to expose a connection to an instance's serial port.

use crate::migrate::MigrateError;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::num::NonZeroUsize;
use std::ops::Range;
use std::sync::Arc;
use std::time::Duration;

use crate::serial::history_buffer::{HistoryBuffer, SerialHistoryOffset};
use futures::future::Fuse;
use futures::stream::SplitSink;
use futures::{FutureExt, SinkExt, StreamExt};
use hyper::upgrade::Upgraded;
use propolis::chardev::{pollers, Sink, Source};
use propolis_client::handmade::api::InstanceSerialConsoleControlMessage;
use slog::{info, warn, Logger};
use thiserror::Error;
use tokio::sync::{mpsc, oneshot, Mutex, RwLock as AsyncRwLock};
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::protocol::{
    frame::coding::CloseCode, CloseFrame,
};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{tungstenite, WebSocketStream};

pub(crate) mod history_buffer;

#[usdt::provider(provider = "propolis")]
mod probes {
    fn serial_close_recv() {}
    fn serial_new_ws() {}
    fn serial_uart_write(n: usize) {}
    fn serial_uart_out() {}
    fn serial_uart_read(n: usize) {}
    fn serial_inject_uart() {}
    fn serial_ws_recv() {}
    fn serial_buffer_size(n: usize) {}
}

/// Errors which may occur during the course of a serial connection.
#[derive(Error, Debug)]
pub enum SerialTaskError {
    #[error("Cannot upgrade HTTP request to WebSockets: {0}")]
    Upgrade(#[from] hyper::Error),

    #[error("WebSocket Error: {0}")]
    WebSocket(#[from] tungstenite::Error),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Mismatched websocket streams while closing")]
    MismatchedStreams,

    #[error("Error while waiting for notification: {0}")]
    OneshotRecv(#[from] oneshot::error::RecvError),

    #[error("JSON marshalling error while processing control message: {0}")]
    Json(#[from] serde_json::Error),
}

pub enum SerialTaskControlMessage {
    Stopping,
    Migration { destination: SocketAddr, from_start: u64 },
}

pub struct SerialTask {
    /// Handle to attached serial session
    pub task: JoinHandle<()>,
    /// Channel used to signal the task to terminate gracefully or notify
    /// clients of a migration
    pub control_ch: mpsc::Sender<SerialTaskControlMessage>,
    /// Channel used to send new client connections to the streaming task
    pub websocks_ch: mpsc::Sender<WebSocketStream<Upgraded>>,
}

pub async fn instance_serial_task<Device: Sink + Source>(
    mut websocks_recv: mpsc::Receiver<WebSocketStream<Upgraded>>,
    mut control_recv: mpsc::Receiver<SerialTaskControlMessage>,
    serial: Arc<Serial<Device>>,
    log: Logger,
) -> Result<(), SerialTaskError> {
    info!(log, "Entered serial task");
    let mut output = [0u8; 1024];
    let mut cur_output: Option<Range<usize>> = None;
    let mut cur_input: Option<(Vec<u8>, usize)> = None;

    let mut ws_sinks: HashMap<
        usize,
        SplitSink<WebSocketStream<Upgraded>, Message>,
    > = HashMap::new();
    let mut ws_streams: HashMap<
        usize,
        futures::stream::SplitStream<WebSocketStream<Upgraded>>,
    > = HashMap::new();

    let (send_ch, mut recv_ch) = mpsc::channel(4);

    let mut next_stream_id = 0usize;

    loop {
        let (uart_read, ws_send) =
            if ws_sinks.is_empty() || cur_output.is_none() {
                (serial.read_source(&mut output).fuse(), Fuse::terminated())
            } else {
                let range = cur_output.clone().unwrap();
                (
                    Fuse::terminated(),
                    if !ws_sinks.is_empty() {
                        futures::stream::iter(
                            ws_sinks.iter_mut().zip(std::iter::repeat(
                                Vec::from(&output[range]),
                            )),
                        )
                        .for_each_concurrent(4, |((_i, ws), bin)| {
                            ws.send(Message::binary(bin)).map(|_| ())
                        })
                        .fuse()
                    } else {
                        Fuse::terminated()
                    },
                )
            };

        let (ws_recv, uart_write) = match &cur_input {
            None => (
                if !ws_streams.is_empty() {
                    futures::stream::iter(ws_streams.iter_mut())
                        .for_each_concurrent(4, |(i, ws)| {
                            ws.next()
                                .then(|msg| send_ch.send((*i, msg)))
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

        let input_recv_ch_fut = recv_ch.recv().fuse();
        let new_ws_recv = websocks_recv.recv().fuse();
        let control_recv_fut = control_recv.recv().fuse();

        tokio::select! {
            // Poll in the order written
            biased;

            // It's important we always poll the close channel first
            // so that a constant stream of incoming/outgoing messages
            // don't cause us to ignore it
            message = control_recv_fut => {
                probes::serial_close_recv!(|| {});
                match message {
                    Some(SerialTaskControlMessage::Stopping) | None => {
                        // Gracefully close the connections to any clients
                        for (i, ws0) in ws_sinks.into_iter() {
                            let ws1 = ws_streams.remove(&i).ok_or(SerialTaskError::MismatchedStreams)?;
                            let mut ws = ws0.reunite(ws1).map_err(|_| SerialTaskError::MismatchedStreams)?;
                            let _ = ws.close(Some(CloseFrame {
                                code: CloseCode::Away,
                                reason: "VM stopped".into(),
                            })).await;
                        }
                    }
                    Some(SerialTaskControlMessage::Migration { destination, from_start }) => {
                        let mut failures = 0;
                        for sink in ws_sinks.values_mut() {
                            if sink.send(Message::Text(serde_json::to_string(
                                &InstanceSerialConsoleControlMessage::Migrating {
                                    destination,
                                    from_start,
                                }
                            )?)).await.is_err() {
                                failures += 1;
                            }
                        }
                        if failures > 0 {
                            warn!(log, "Failed to send migration info to {} connected clients.", failures);
                        }
                    }
                }
                info!(log, "Terminating serial task");
                break;
            }

            new_ws = new_ws_recv => {
                probes::serial_new_ws!(|| {});
                if let Some(ws) = new_ws {
                    let (ws_sink, ws_stream) = ws.split();
                    ws_sinks.insert(next_stream_id, ws_sink);
                    ws_streams.insert(next_stream_id, ws_stream);
                    next_stream_id += 1;
                }
            }

            // Write bytes into the UART from the WS
            written = uart_write => {
                probes::serial_uart_write!(|| { written.unwrap_or(0) });
                match written {
                    Some(0) | None => {
                        break;
                    }
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
                probes::serial_uart_out!(|| {});
                cur_output = None;
            }

            // Read bytes from the UART to be transmitted out the WS
            nread = uart_read => {
                // N.B. Putting this probe inside the match arms below causes
                //      the `break` arm to be taken unexpectedly. See
                //      propolis#292 for details.
                probes::serial_uart_read!(|| { nread.unwrap_or(0) });
                match nread {
                    Some(0) | None => {
                        break;
                    }
                    Some(n) => {
                        cur_output = Some(0..n)
                    }
                }
            }

            // Receive bytes from the intermediate channel to be injected into
            // the UART. This needs to be checked before `ws_recv` so that
            // "close" messages can be processed and their indicated
            // sinks/streams removed before they are polled again.
            pair = input_recv_ch_fut => {
                probes::serial_inject_uart!(|| {});
                if let Some((i, msg)) = pair {
                    match msg {
                        Some(Ok(Message::Binary(input))) => {
                            cur_input = Some((input, 0));
                        }
                        Some(Ok(Message::Close(..))) | None => {
                            info!(log, "Removing closed serial connection {}.", i);
                            let sink = ws_sinks.remove(&i).ok_or(SerialTaskError::MismatchedStreams)?;
                            let stream = ws_streams.remove(&i).ok_or(SerialTaskError::MismatchedStreams)?;
                            if let Err(e) = sink.reunite(stream).map_err(|_| SerialTaskError::MismatchedStreams)?.close(None).await {
                                warn!(log, "Failed while closing stream {}: {}", i, e);
                            }
                        },
                        _ => continue,
                    }
                }
            }

            // Receive bytes from connected WS clients to feed to the
            // intermediate recv_ch
            _ = ws_recv => {
                probes::serial_ws_recv!(|| {});
            }
        }
    }
    info!(log, "Returning from serial task");
    Ok(())
}

/// Represents a serial connection into the VM.
pub struct Serial<Device: Sink + Source> {
    uart: Arc<Device>,

    task_control_ch: Mutex<Option<mpsc::Sender<SerialTaskControlMessage>>>,

    sink_poller: Arc<pollers::SinkBuffer>,
    source_poller: Arc<pollers::SourceBuffer>,
    history: AsyncRwLock<HistoryBuffer>,
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
        let history = Default::default();
        sink_poller.attach(uart.as_ref());
        source_poller.attach(uart.as_ref());
        uart.set_autodiscard(false);

        let task_control_ch = Default::default();

        Serial { uart, task_control_ch, sink_poller, source_poller, history }
    }

    pub async fn read_source(&self, buf: &mut [u8]) -> Option<usize> {
        let uart = self.uart.clone();
        let bytes_read = self.source_poller.read(buf, uart.as_ref()).await?;
        self.history.write().await.consume(&buf[..bytes_read]);
        Some(bytes_read)
    }

    pub async fn write_sink(&self, buf: &[u8]) -> Option<usize> {
        let uart = self.uart.clone();
        self.sink_poller.write(buf, uart.as_ref()).await
    }

    pub(crate) async fn history_vec(
        &self,
        byte_offset: SerialHistoryOffset,
        max_bytes: Option<usize>,
    ) -> Result<(Vec<u8>, usize), history_buffer::Error> {
        self.history.read().await.contents_vec(byte_offset, max_bytes)
    }

    // provide the channel through which we inform connected websocket clients
    // that a migration has occurred, and where to reconnect.
    // (the server's serial-to-websocket task -- and thus the receiving end of
    // this channel -- are spawned in `instance_ensure_common`, after the
    // construction of `Serial`)
    pub(crate) async fn set_task_control_sender(
        &self,
        control_ch: mpsc::Sender<SerialTaskControlMessage>,
    ) {
        self.task_control_ch.lock().await.replace(control_ch);
    }

    pub(crate) async fn export_history(
        &self,
        destination: SocketAddr,
    ) -> Result<String, MigrateError> {
        let read_hist = self.history.read().await;
        let from_start = read_hist.bytes_from_start() as u64;
        let encoded = ron::to_string(&*read_hist)
            .map_err(|e| MigrateError::Codec(e.to_string()))?;
        drop(read_hist);
        if let Some(ch) = self.task_control_ch.lock().await.as_ref() {
            ch.send(SerialTaskControlMessage::Migration {
                destination,
                from_start,
            })
            .await
            .map_err(|_| MigrateError::InvalidInstanceState)?;
        }
        Ok(encoded)
    }

    pub(crate) async fn import(
        &self,
        serialized_hist: &str,
    ) -> Result<(), MigrateError> {
        self.sink_poller.attach(self.uart.as_ref());
        self.source_poller.attach(self.uart.as_ref());
        self.uart.set_autodiscard(false);
        let decoded = ron::from_str(serialized_hist)
            .map_err(|e| MigrateError::Codec(e.to_string()))?;
        let mut write_hist = self.history.write().await;
        *write_hist = decoded;
        Ok(())
    }
}

impl<Device: Sink + Source> Drop for Serial<Device> {
    fn drop(&mut self) {
        self.uart.set_autodiscard(true);
    }
}
