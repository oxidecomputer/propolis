// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Interfaces to access a guest's serial console.

use anyhow::Result;
use camino::Utf8PathBuf;
use futures::{SinkExt, StreamExt};
use reqwest::Upgraded;
use tokio::{
    sync::{
        mpsc::{UnboundedReceiver, UnboundedSender},
        oneshot,
    },
    task::JoinHandle,
};
use tokio_tungstenite::{
    tungstenite::{protocol::Role, Message},
    WebSocketStream,
};
use tracing::{debug, error, info};

mod raw_buffer;

/// Describes a request to wait for a string to appear on the serial console or
/// in the console's back buffer.
struct OutputWaiter {
    /// The string this waiter should wait for.
    wanted: String,

    /// When the wait is satisfied, send the contents of the buffer prior to
    /// (and exclusive of) the waited-for string to this channel.
    preceding_tx: oneshot::Sender<String>,
}

/// An interface for objects that handle and buffer characters and commands a
/// guest writes to its serial console.
trait Buffer: Send {
    /// Processes the supplied `bytes` as input to the buffer.
    fn process_bytes(&mut self, bytes: &[u8]);

    /// Registers a new request to wait for a string to appear in the buffer.
    fn register_wait_for_output(&mut self, waiter: OutputWaiter);

    /// Ensures there is no active request to wait for a string to appear in
    /// this buffer. Returns the previous active request if there was one.
    fn cancel_wait_for_output(&mut self) -> Option<OutputWaiter>;
}

/// The kind of buffering discipline to use for a guest's serial output.
pub enum BufferKind {
    /// Assume that the guest will output characters and command bytes (like
    /// carriage returns and line feeds) "in the raw" without trying to
    /// implement its own buffering or scrollback.
    Raw,

    #[allow(dead_code)]
    /// Assume that the guest believes it is sending commands to drive a
    /// VT100-compatible 80x24 terminal and emulate that terminal.
    Vt80x24,
}

/// The set of commands that the serial console can send to its processing task.
enum TaskCommand {
    /// Send the supplied bytes to the VM.
    SendBytes(Vec<u8>),

    /// Register to be notified if and when a supplied string appears in the
    /// serial console's buffer.
    RegisterWait(OutputWaiter),

    /// Cancel any outstanding wait for bytes to appear in the buffer.
    CancelWait,
}

/// A connection to a guest serial console made available on a particular guest
/// serial port.
pub struct SerialConsole {
    /// A handle to a tokio task that handles websocket messages to and from the
    /// Propolis server that serves this console.
    ws_task: JoinHandle<()>,

    /// Used to send commands to the worker thread for this console.
    cmd_tx: UnboundedSender<TaskCommand>,
}

impl SerialConsole {
    /// Creates a new serial console connection.
    ///
    /// # Arguments
    ///
    /// - `serial_conn`: An upgraded websocket connection obtained from
    ///   successfully connecting to Propolis's serial console API.
    /// - `buffer_kind`: Supplies the buffering discipline to start with.
    pub async fn new(
        serial_conn: Upgraded,
        buffer_kind: BufferKind,
        log_path: Utf8PathBuf,
    ) -> Result<Self> {
        let ws =
            WebSocketStream::from_raw_socket(serial_conn, Role::Client, None)
                .await;

        let (cmd_tx, cmd_rx) = tokio::sync::mpsc::unbounded_channel();
        let ws_task = tokio::spawn(serial_task(
            ws,
            new_buffer(buffer_kind, log_path)?,
            cmd_rx,
        ));

        Ok(Self { ws_task, cmd_tx })
    }

    /// Directs the console worker thread to send the supplied `bytes` to the
    /// guest.
    pub fn send_bytes(&self, bytes: Vec<u8>) -> anyhow::Result<()> {
        self.cmd_tx.send(TaskCommand::SendBytes(bytes))?;
        Ok(())
    }

    /// Registers with the current buffer a request to wait for `wanted` to
    /// appear in the console buffer. When a match is found, the buffer sends
    /// all buffered characters preceding the match to `preceding_tx`, consuming
    /// those characters and the matched string. If the buffer already contains
    /// one or more matches at the time the waiter is registered, the last match
    /// is used to satisfy the wait immediately.
    pub fn register_wait_for_string(
        &self,
        wanted: String,
        preceding_tx: oneshot::Sender<String>,
    ) -> Result<()> {
        self.cmd_tx.send(TaskCommand::RegisterWait(OutputWaiter {
            wanted,
            preceding_tx,
        }))?;
        Ok(())
    }

    /// Cancels the outstanding wait on the current buffer, if there was one.
    pub fn cancel_wait_for_string(&self) -> Result<()> {
        self.cmd_tx.send(TaskCommand::CancelWait)?;
        Ok(())
    }
}

impl Drop for SerialConsole {
    fn drop(&mut self) {
        self.ws_task.abort();
    }
}

/// Creates a new serial console buffer of the supplied kind.
fn new_buffer(
    kind: BufferKind,
    log_path: Utf8PathBuf,
) -> Result<Box<dyn Buffer>> {
    match kind {
        BufferKind::Raw => Ok(Box::new(raw_buffer::RawBuffer::new(log_path)?)),
        BufferKind::Vt80x24 => unimplemented!(
            "80x24 terminal emulation not yet implemented, see propolis#601"
        ),
    }
}

/// Runs the serial websocket connection processing loop.
///
/// # Arguments
///
/// - `ws`: A bidirectional stream constructed over a websocket connection to
///   the target Propolis serial console.
/// - `buffer`: A reference to the buffer object backing this serial console.
///   The task posts newly-written bytes from the guest back to this buffer.
/// - `input_rx`: Receives bytes from a serial console's owner to send out to
///   the target Propolis's serial console.
#[tracing::instrument(level = "info", name = "serial console task", skip_all)]
async fn serial_task(
    mut ws: WebSocketStream<Upgraded>,
    mut buffer: Box<dyn Buffer>,
    mut cmd_rx: UnboundedReceiver<TaskCommand>,
) {
    loop {
        tokio::select! {
            cmd = cmd_rx.recv() => {
                let Some(cmd) = cmd else {
                    debug!("serial console command channel was closed");
                    break;
                };
                match cmd {
                    TaskCommand::SendBytes(bytes) => {
                        if let Err(e) = ws.send(Message::Binary(bytes)).await {
                            error!(
                                ?e,
                                "failed to send input to serial console websocket"
                            );
                        }
                    }
                    TaskCommand::RegisterWait(waiter) => {
                        buffer.register_wait_for_output(waiter);
                    }
                    TaskCommand::CancelWait => {
                        buffer.cancel_wait_for_output();
                    }
                }
            }
            msg = ws.next() => {
                match msg {
                    Some(Ok(Message::Binary(bytes))) => {
                        buffer.process_bytes(&bytes);
                    }
                    Some(Ok(Message::Close(..))) => {
                        debug!("serial websocket closed");
                        break;
                    }
                    Some(Ok(Message::Text(s))) => {
                        info!(s, "serial socket control message");
                    }
                    None => {
                        info!("serial websocket closed unexpectedly");
                        break;
                    }
                    _ => continue,
                }
            },

        };
    }
}
