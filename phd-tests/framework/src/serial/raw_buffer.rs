// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Implements a "raw" buffer for serial console output that processes
//! characters and newlines but ignores VT100 control characters.

use super::{Buffer, OutputWaiter};

/// Wraps up the contents of a raw buffer in a single struct that can implement
/// [`vte::Perform`]. This allows the "outer" buffer to contain a
/// [`vte::Parser`] and call its `advance` method with a mutable reference to
/// the buffer contents as the first argument without running afoul of mutable
/// borrowing rules.
#[derive(Default)]
struct Inner {
    wait_buffer: String,
    waiter: Option<OutputWaiter>,
}

impl Inner {
    /// Pushes `c` to the buffer's contents and attempts to satisfy active
    /// waits.
    fn push_character(&mut self, c: char) {
        self.wait_buffer.push(c);
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
        if let Some(idx) = self.wait_buffer.rfind(&waiter.wanted) {
            // Send all of the data in the buffer prior to the target string
            // out the waiter's channel.
            //
            // Because incoming bytes from Propolis may be processed on a
            // separate task than the task that registered the wait, this
            // can race such that the wait is satisfied just as the waiter
            // times out and closes its half of the channel. There's nothing
            // to be done about this, so just ignore any errors here.
            let out = self.wait_buffer.drain(..idx).collect();
            let _ = waiter.preceding_tx.send(out);

            // Clear the matched string out of the wait buffer.
            self.wait_buffer = self.wait_buffer.split_off(waiter.wanted.len());
        } else {
            self.waiter = Some(waiter);
        }
    }
}

impl vte::Perform for Inner {
    fn print(&mut self, c: char) {
        self.push_character(c);
    }

    fn execute(&mut self, byte: u8) {
        if byte == b'\n' {
            self.push_character('\n');
        }
    }
}

/// A "raw" serial console buffer that handles incoming characters and newline
/// control bytes and nothing else.
pub(super) struct RawBuffer {
    inner: Inner,
    parser: vte::Parser,
}

impl RawBuffer {
    /// Constructs a new buffer.
    pub(super) fn new() -> Self {
        Self { inner: Inner::default(), parser: vte::Parser::new() }
    }
}

impl Buffer for RawBuffer {
    fn process_bytes(&mut self, bytes: &[u8]) {
        for b in bytes {
            self.parser.advance(&mut self.inner, *b);
        }
    }

    fn register_wait_for_output(&mut self, waiter: OutputWaiter) {
        self.inner.satisfy_or_set_wait(waiter);
    }

    fn cancel_wait_for_output(&mut self) -> Option<OutputWaiter> {
        self.inner.waiter.take()
    }
}
