use std::sync::Arc;

use anyhow::Result;
use thiserror::Error;
use tokio::{
    sync::{mpsc, Mutex},
    task::JoinHandle,
};
use tracing::{debug, info, info_span, Instrument};

#[derive(Error, Debug)]
pub enum Vt100Error {
    #[error("Waiter already registered (wants `{0}`)")]
    WaiterAlreadyRegistered(String),
}

/// The wrapper struct for the VT-100 processor. This takes a channel that
/// receives bytes from the serial console websocket, interprets them as VT-100
/// characters, and converts them into text usable by higher-level serial
/// console abstractions like "wait for this output".
pub struct Vt100Processor {
    /// A handle to the task that receives bytes from the guest serial console.
    task: JoinHandle<()>,

    /// State shared between the receiver task and the public interface to the
    /// processor.
    state: Arc<SharedState>,
}

impl Vt100Processor {
    pub fn new(vt_rx: mpsc::Receiver<Vec<u8>>) -> Self {
        let vt_span = info_span!("Serial");
        vt_span.follows_from(tracing::Span::current());

        let state = Arc::new(SharedState {
            span: vt_span.clone(),
            inner: Mutex::default(),
        });

        let state_for_task = state.clone();
        let task = tokio::spawn(
            async move {
                vt100_handler(state_for_task, vt_rx).await;
            }
            .instrument(vt_span),
        );

        Self { task, state }
    }

    pub async fn register_wait_for_string(
        &self,
        wanted: String,
        preceding_tx: mpsc::Sender<String>,
    ) -> Result<()> {
        self.state.register_wait_for_string(wanted, preceding_tx).await
    }

    pub async fn cancel_wait_for_string(&self) {
        self.state.cancel_wait_for_string().await
    }
}

impl Drop for Vt100Processor {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn vt100_handler(
    state: Arc<SharedState>,
    mut vt_rx: mpsc::Receiver<Vec<u8>>,
) {
    let mut parser = vte::Parser::new();
    let mut performer = VtPerformer { last_char: None };
    loop {
        tokio::select! {
            bytes = vt_rx.recv() => {
                match bytes {
                    Some(bytes) => {
                        for b in bytes {
                            performer.last_char = None;
                            parser.advance(&mut performer, b);
                            if let Some(c) = performer.last_char {
                                state.push_character(c).await;
                            }
                        }
                    },
                    None => break,
                }
            }
        }
    }
}

/// A registered waiter who wants a specific line to appear at the end of the
/// serial console output.
struct Waiter {
    wanted: String,
    output_tx: mpsc::Sender<String>,
}

/// State shared between the receiver task and the public interface to the VT
/// processor.
struct SharedState {
    span: tracing::Span,
    inner: Mutex<Inner>,
}

impl SharedState {
    async fn push_character(&self, c: char) {
        let mut guard = self.inner.lock().await;
        match c {
            '\n' => {
                info!("{}", guard.next_output_line);
                guard.next_output_line.clear();
            }
            _ => {
                guard.next_output_line.push(c);
            }
        }
        guard.wait_buffer.push(c);

        let waiter = guard.waiter.take();
        if let Some(waiter) = waiter {
            if guard.wait_buffer.ends_with(&waiter.wanted) {
                let new_length = guard.wait_buffer.len() - waiter.wanted.len();
                guard.wait_buffer.truncate(new_length);
                let out = guard.wait_buffer.drain(..).collect();

                // This can race such that the last character satisfying a wait
                // arrives just as the waiter times out and closes its half of
                // the channel. There's nothing to be done about this, so ignore
                // any errors here.
                let _ = waiter.output_tx.send(out).await;
            } else {
                guard.waiter = Some(waiter);
            }
        }
    }

    async fn register_wait_for_string(
        &self,
        wanted: String,
        output_tx: mpsc::Sender<String>,
    ) -> Result<()> {
        let _span = self.span.enter();
        info!(wanted, "Registering wait for serial console output");
        let mut guard = self.inner.lock().await;

        if let Some(w) = &mut guard.waiter {
            Err(Vt100Error::WaiterAlreadyRegistered(w.wanted.clone()).into())
        } else if let Some(idx) = guard.wait_buffer.find(wanted.as_str()) {
            let remainder = guard.wait_buffer.split_off(idx + wanted.len());
            let out = guard.wait_buffer.drain(..idx).collect();
            guard.wait_buffer = remainder;

            // The channel is not guaranteed to have come from the same task as
            // the one that's registering the wait, so it may be closed by the
            // other task before this message can be sent. There's nothing to be
            // done about this, so ignore errors here.
            let _ = output_tx.send(out).await;
            Ok(())
        } else {
            guard.waiter = Some(Waiter { wanted, output_tx });
            Ok(())
        }
    }

    async fn cancel_wait_for_string(&self) {
        let mut guard = self.inner.lock().await;
        guard.waiter.take();
    }
}

#[derive(Default)]
struct Inner {
    next_output_line: String,
    wait_buffer: String,
    waiter: Option<Waiter>,
}

impl Drop for Inner {
    fn drop(&mut self) {
        debug!(
            self.next_output_line,
            "Dropped serial console with partial line"
        );
    }
}

/// A helper for using the `vte` crate's `Perform` trait.
///
/// The VT parser maintains a small state machine that receives individual bytes
/// from a VT-compatible byte source. When the state machine rolls around to a
/// specific action (print a character, execute a control code command, etc.),
/// it calls back into routines on the specified performer.
///
/// The performer callbacks are not async, so even if they're called from an
/// async context, the callbacks can't do anything that would require awaiting a
/// result (e.g. writing to a channel). To handle this, the perform just stashes
/// the results of the callbacks it receives and returns, allowing the caller to
/// extract and deal with those results.
struct VtPerformer {
    last_char: Option<char>,
}

impl vte::Perform for VtPerformer {
    fn print(&mut self, c: char) {
        self.last_char = Some(c);
    }

    fn execute(&mut self, b: u8) {
        if b == b'\n' {
            self.last_char = Some('\n');
        }
    }
}
