// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Interfaces to access a guest's serial console.

use anyhow::Result;
use camino::{Utf8Path, Utf8PathBuf};
use futures::SinkExt;
use propolis_client::support::InstanceSerialConsoleHelper;
use tokio::sync::{
    mpsc::{UnboundedReceiver, UnboundedSender},
    oneshot,
};
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, error, info};

mod raw_buffer;
mod vt80x24;

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

    /// Clears the unprocessed contents of the buffer.
    fn clear(&mut self);

    /// Registers a new request to wait for a string to appear in the buffer.
    fn register_wait_for_output(&mut self, waiter: OutputWaiter);

    /// Ensures there is no active request to wait for a string to appear in
    /// this buffer. Returns the previous active request if there was one.
    fn cancel_wait_for_output(&mut self) -> Option<OutputWaiter>;
}

/// The kind of buffering discipline to use for a guest's serial output.
#[derive(Debug)]
pub enum BufferKind {
    /// Assume that the guest will output characters and command bytes (like
    /// carriage returns and line feeds) "in the raw" without trying to
    /// implement its own buffering or scrollback.
    Raw,

    /// Assume that the guest believes it is sending commands to drive a
    /// VT100-compatible 80x24 terminal and emulate that terminal.
    Vt80x24,
}

/// The set of commands that the serial console can send to its processing task.
enum TaskCommand {
    /// Send the supplied bytes to the VM.
    SendBytes { bytes: Vec<u8>, done: oneshot::Sender<()> },

    /// Clears the contents of the task's console buffer. This does not cancel
    /// the active wait, if there is one.
    Clear,

    /// Register to be notified if and when a supplied string appears in the
    /// serial console's buffer.
    RegisterWait(OutputWaiter),

    /// Cancel any outstanding wait for bytes to appear in the buffer.
    CancelWait,

    /// Change the buffer kind to the supplied kind. Note that this command
    /// discards the current buffer's contents and cancels any active waits.
    ChangeBufferKind(BufferKind),

    /// Insert the supplied delay between each byte written to the serial
    /// console (to avoid keyboard debouncing logic in the guest). If the delay
    /// is set to 0, the serial task will send Vecs of bytes to the guest in a
    /// single message.
    SetGuestWriteDelay(std::time::Duration),
}

/// A connection to a guest serial console made available on a particular guest
/// serial port.
#[derive(Clone)]
pub struct SerialConsole {
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
        serial_helper: InstanceSerialConsoleHelper,
        buffer_kind: BufferKind,
        log_path: Utf8PathBuf,
    ) -> Result<Self> {
        let (cmd_tx, cmd_rx) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(serial_task(serial_helper, buffer_kind, log_path, cmd_rx));

        Ok(Self { cmd_tx })
    }

    /// Directs the console worker thread to send the supplied `bytes` to the
    /// guest. Returns a `oneshot::Receiver` that the console worker thread
    /// signals once all the bytes have been set.
    pub fn send_bytes(
        &self,
        bytes: Vec<u8>,
    ) -> anyhow::Result<oneshot::Receiver<()>> {
        let (done, done_rx) = oneshot::channel();
        self.cmd_tx.send(TaskCommand::SendBytes { bytes, done })?;
        Ok(done_rx)
    }

    /// Directs the console worker thread to clear the serial console buffer.
    pub fn clear(&self) -> anyhow::Result<()> {
        self.cmd_tx.send(TaskCommand::Clear)?;
        Ok(())
    }

    /// Registers with the current buffer a request to wait for `wanted` to
    /// appear in the console buffer. When a match is found, the buffer sends
    /// all buffered characters preceding the match to `preceding_tx`. If the
    /// buffer already contains one or more matches at the time the waiter is
    /// registered, the last match is used to satisfy the wait immediately.
    ///
    /// Note that this function *does not* clear any characters from the buffer.
    /// Callers who want to retire previously-echoed characters in the buffer
    /// must explicitly call `clear`.
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

    /// Changes the buffering discipline for this console.
    pub fn change_buffer_kind(&self, kind: BufferKind) -> Result<()> {
        self.cmd_tx.send(TaskCommand::ChangeBufferKind(kind))?;
        Ok(())
    }

    /// Sets the delay to insert between sending individual bytes to the guest.
    pub fn set_repeated_character_debounce(
        &self,
        delay: std::time::Duration,
    ) -> Result<()> {
        self.cmd_tx.send(TaskCommand::SetGuestWriteDelay(delay))?;
        Ok(())
    }
}

/// Creates a new serial console buffer of the supplied kind.
fn new_buffer(
    kind: BufferKind,
    log_path: impl AsRef<Utf8Path>,
) -> Result<Box<dyn Buffer>> {
    match kind {
        BufferKind::Raw => Ok(Box::new(raw_buffer::RawBuffer::new(
            log_path.as_ref().to_path_buf(),
        )?)),
        BufferKind::Vt80x24 => Ok(Box::new(vt80x24::Vt80x24::new())),
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
    mut stream: InstanceSerialConsoleHelper,
    initial_buffer_kind: BufferKind,
    log_path: Utf8PathBuf,
    mut cmd_rx: UnboundedReceiver<TaskCommand>,
) {
    let mut buffer = new_buffer(initial_buffer_kind, &log_path).unwrap();
    let mut debounce = std::time::Duration::from_secs(0);
    loop {
        tokio::select! {
            cmd = cmd_rx.recv() => {
                let Some(cmd) = cmd else {
                    debug!("serial console command channel was closed");
                    break;
                };
                match cmd {
                    TaskCommand::SendBytes { bytes, done } => {
                        if debounce.is_zero() {
                            if let Err(e) = stream.send(Message::Binary(bytes)).await {
                                error!(
                                    ?e,
                                    "failed to send input to serial console websocket"
                                );
                            }
                        } else {
                            let mut bytes = bytes.iter().peekable();
                            while let Some(b) = bytes.next() {
                                if let Err(e) = stream.send(Message::Binary(vec![*b])).await {
                                    error!(
                                        ?e,
                                        "failed to send input to serial console websocket"
                                    );
                                }

                                if let Some(next) = bytes.peek() {
                                    if *next == b {
                                        tokio::time::sleep(debounce).await;
                                    }
                                }
                            }
                        }

                        let _ = done.send(());
                    }
                    TaskCommand::Clear => buffer.clear(),
                    TaskCommand::RegisterWait(waiter) => {
                        buffer.register_wait_for_output(waiter);
                    }
                    TaskCommand::CancelWait => {
                        buffer.cancel_wait_for_output();
                    }
                    TaskCommand::ChangeBufferKind(kind) => {
                        buffer = new_buffer(kind, &log_path).unwrap();
                    }
                    TaskCommand::SetGuestWriteDelay(delay) => {
                        debounce = delay;
                    }
                }
            }
            msg = stream.recv() => {
                let Some(Ok(msg)) = msg else {
                    info!("serial websocket closed unexpectedly");
                    break;
                };

                match msg.process().await {
                    Ok(Message::Binary(bytes)) => {
                        buffer.process_bytes(&bytes);
                    }
                    Ok(Message::Close(..)) => {
                        debug!("serial websocket closed");
                        break;
                    }
                    Ok(Message::Text(s)) => {
                        info!(s, "serial socket control message");
                    }
                    _ => continue,
                }
            },

        };
    }
}
