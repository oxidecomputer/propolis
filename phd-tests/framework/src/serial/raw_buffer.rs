// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Implements a "raw" buffer for serial console output that processes
//! characters and newlines but ignores VT100 control characters.

use std::io::{BufWriter, Write};

use anyhow::{Context, Result};
use camino::Utf8PathBuf;
use termwiz::escape::{
    csi::{Cursor::Right, CSI},
    parser::Parser,
};
use tracing::{error, trace};

use super::{Buffer, OutputWaiter};

/// A "raw" serial console buffer that handles incoming characters and newline
/// control bytes and nothing else.
pub(super) struct RawBuffer {
    log: std::io::BufWriter<std::fs::File>,
    line_buffer: String,
    wait_buffer: String,
    waiter: Option<OutputWaiter>,
    parser: Parser,
}

impl RawBuffer {
    /// Constructs a new buffer.
    pub(super) fn new(log_path: Utf8PathBuf) -> Result<Self> {
        let log_file = std::fs::File::create(&log_path).with_context(|| {
            format!("opening serial console log file {}", log_path)
        })?;
        let writer = BufWriter::new(log_file);
        Ok(Self {
            log: writer,
            line_buffer: String::new(),
            wait_buffer: String::new(),
            waiter: None,
            parser: Parser::new(),
        })
    }

    /// Pushes `c` to the buffer's contents and attempts to satisfy active
    /// waits.
    fn push_character(&mut self, c: char) {
        if c == '\n' {
            self.log.write_all(self.line_buffer.as_bytes()).unwrap();
            self.log.write_all(b"\n").unwrap();
            self.log.flush().unwrap();
            self.line_buffer.clear();
        } else {
            self.line_buffer.push(c);
        }

        self.wait_buffer.push(c);
        if let Some(waiter) = self.waiter.take() {
            self.satisfy_or_set_wait(waiter);
        }
    }

    /// Pushes `s` to the buffer's contents and attempts to satisfy active
    /// waits. `s` is presumed not to contain any control characters.
    fn push_str(&mut self, s: &str) {
        self.line_buffer.push_str(s);
        self.wait_buffer.push_str(s);
        if let Some(waiter) = self.waiter.take() {
            self.satisfy_or_set_wait(waiter);
        }
    }

    /// Attempts to satisfy the wait described by `waiter` or, if the wait
    /// cannot yet be satisfied, stores it to be checked again later.
    ///
    /// A wait is satisfied if the `wait_buffer` contains the string in the
    /// supplied waiter. When this happens, all of the characters preceding the
    /// match are sent to the output channel in the supplied `waiter`, the
    /// matching characters are removed, and the remainder of the wait buffer
    /// is preserved.
    ///
    /// If the buffer contains multiple matches, the *last* match is used to
    /// satisfy the wait.
    ///
    /// # Panics
    ///
    /// Panics if a wait is already set (irrespective of whether the new wait
    /// actually needs to be stored).
    fn satisfy_or_set_wait(&mut self, waiter: OutputWaiter) {
        assert!(self.waiter.is_none());
        trace!(
            contents = self.wait_buffer,
            target = waiter.wanted,
            "checking wait on raw serial buffer"
        );
        if let Some(idx) = self.wait_buffer.rfind(&waiter.wanted) {
            let out = self.wait_buffer.drain(..idx).collect();
            self.wait_buffer = self.wait_buffer.split_off(waiter.wanted.len());

            // Because incoming bytes from Propolis may be processed on a
            // separate task than the task that registered the wait, this
            // can race such that the wait is satisfied just as the waiter
            // times out and closes its half of the channel. There's nothing
            // to be done about this, so just ignore any errors here.
            let _ = waiter.preceding_tx.send(out);
        } else {
            self.waiter = Some(waiter);
        }
    }
}

impl Buffer for RawBuffer {
    fn process_bytes(&mut self, bytes: &[u8]) {
        use termwiz::escape::{Action, ControlCode};
        let actions = self.parser.parse_as_vec(bytes);
        for action in actions {
            match action {
                Action::Print(c) => self.push_character(c),
                Action::PrintString(s) => {
                    self.push_str(&s);
                }
                Action::Control(ControlCode::LineFeed) => {
                    self.push_character('\n');
                }
                Action::CSI(CSI::Cursor(Right(n))) => {
                    self.push_str(&" ".repeat(n as usize));
                }
                _ => {
                    trace!(?action, "raw buffer ignored action");
                }
            }
        }
    }

    fn clear(&mut self) {
        self.wait_buffer.clear();
    }

    fn register_wait_for_output(&mut self, waiter: OutputWaiter) {
        self.satisfy_or_set_wait(waiter);
    }

    fn cancel_wait_for_output(&mut self) -> Option<OutputWaiter> {
        self.waiter.take()
    }
}

impl Drop for RawBuffer {
    fn drop(&mut self) {
        if let Err(e) = self.log.flush() {
            error!(%e, "failed to flush serial console log during drop");
        }
    }
}

#[cfg(test)]
mod test {
    use tokio::sync::oneshot;

    use super::*;

    fn make_buffer() -> RawBuffer {
        let file =
            std::fs::OpenOptions::new().write(true).open("/dev/null").unwrap();

        RawBuffer {
            log: std::io::BufWriter::new(file),
            line_buffer: String::new(),
            wait_buffer: String::new(),
            waiter: None,
            parser: Parser::new(),
        }
    }

    #[tokio::test]
    async fn successful_wait_consumes_buffer_contents() {
        let mut buf = make_buffer();
        let (tx, mut rx) = oneshot::channel();
        buf.push_str("the quick brown fox jumped over the lazy propolis");
        buf.satisfy_or_set_wait(OutputWaiter {
            wanted: "jumped over".to_string(),
            preceding_tx: tx,
        });
        assert_eq!(rx.try_recv().unwrap(), "the quick brown fox ");
        assert_eq!(buf.wait_buffer, " the lazy propolis");

        // Repeat the test, but register the wait before the characters are
        // pushed.
        buf.clear();
        let (tx, mut rx) = oneshot::channel();
        buf.satisfy_or_set_wait(OutputWaiter {
            wanted: "jumped over".to_string(),
            preceding_tx: tx,
        });
        buf.push_str("the quick brown fox jumped over the lazy propolis");
        assert_eq!(rx.try_recv().unwrap(), "the quick brown fox ");
        assert_eq!(buf.wait_buffer, " the lazy propolis");
    }

    #[tokio::test]
    async fn successful_wait_consumes_last_match() {
        let mut buf = make_buffer();
        let (tx, mut rx) = oneshot::channel();
        buf.push_str(
            "I put some Oxide in your Oxide so you can Oxide while you Oxide",
        );
        buf.satisfy_or_set_wait(OutputWaiter {
            wanted: "you".to_string(),
            preceding_tx: tx,
        });
        assert_eq!(
            rx.try_recv().unwrap(),
            "I put some Oxide in your Oxide so you can Oxide while "
        );
        assert_eq!(buf.wait_buffer, " Oxide");
    }
}
