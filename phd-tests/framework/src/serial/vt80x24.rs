// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use termwiz::{
    color::ColorAttribute,
    escape::{
        csi::{Cursor, Edit, EraseInLine, CSI},
        parser::Parser,
        Action, ControlCode,
    },
    surface::{Change, Position, Surface},
};
use tracing::trace;

use super::{Buffer, OutputWaiter};

/// Simulates a VT100-compatible 80-by-24 terminal to buffer console output from
/// guests that assume they are driving such a terminal.
pub(super) struct Vt80x24 {
    /// Contains the currently registered request to wait for a string to appear
    /// on the virtual string, if such a request exists.
    ///
    /// This buffer does not support text wrapping; that is, a wait for a string
    /// longer than 80 characters with no intervening newline will never be
    /// satisfied, because all lines in the virtual buffer implicitly break at
    /// 80 characters.
    waiter: Option<OutputWaiter>,

    /// The virtual terminal contents, represented as 24 rows of 80 character
    /// cells each.
    surface: Surface,

    /// The parsing state machine that converts incoming bytes into VT100
    /// commands.
    parser: Parser,
}

impl Vt80x24 {
    pub(super) fn new() -> Self {
        Self {
            waiter: None,
            parser: Parser::new(),
            surface: Surface::new(80, 24),
        }
    }

    /// Converts a list of VT100 actions into a set of changes to the 80x24
    /// buffer, applies those changes, and attempts to satisfy the active wait.
    fn apply_actions(&mut self, actions: &[Action]) {
        // Unfortunately, there is no one-to-one mapping between VT100 actions
        // and `Change`s to a termwiz `Surface`. The match below is enough to
        // make buffering work for simple terminals that only print characters,
        // position the cursor, and use the 'erase to end of line' VT100
        // command. These commands are (fortunately) each representable in a
        // single `Change`. More complex commands (e.g. 'erase to start of
        // line') will have to be composed of multiple `Change`s if support for
        // them is needed.
        let to_change = |action: &Action| -> Option<Change> {
            let change = match action {
                Action::Print(c) => Some(Change::from(*c)),
                Action::PrintString(s) => Some(Change::from(s)),
                Action::Control(ctrl) => match ctrl {
                    ControlCode::LineFeed => Some(Change::from('\n')),
                    ControlCode::CarriageReturn => Some(Change::from('\r')),
                    _ => None,
                },
                Action::CSI(csi) => match csi {
                    CSI::Cursor(Cursor::Position { line, col }) => {
                        Some(make_absolute_cursor_position(
                            col.as_zero_based() as usize,
                            line.as_zero_based() as usize,
                        ))
                    }
                    CSI::Edit(Edit::EraseInLine(
                        EraseInLine::EraseToEndOfLine,
                    )) => {
                        Some(Change::ClearToEndOfLine(ColorAttribute::Default))
                    }
                    _ => None,
                },
                _ => None,
            };

            trace!(?action, ?change, "termwiz VT100 action");
            change
        };

        let changes = actions.iter().filter_map(to_change).collect();
        let seq = self.surface.add_changes(changes);
        self.surface.flush_changes_older_than(seq);

        if let Some(waiter) = self.waiter.take() {
            self.satisfy_or_set_wait(waiter);
        }
    }

    /// Attempts to satisfy the wait described by `waiter`. If the wait is not
    /// immediately satisfiable, stores `waiter` to try again later.
    fn satisfy_or_set_wait(&mut self, waiter: OutputWaiter) {
        assert!(self.waiter.is_none());
        let mut contents = self.surface.screen_chars_to_string();
        trace!(?contents, "termwiz contents");
        if let Some(idx) = contents.rfind(&waiter.wanted) {
            // Callers who set waits assume that matched strings are consumed
            // from the buffer and won't match again unless the string is
            // rendered to the buffer again. In the raw buffering case, this is
            // straightforward: the buffer is already a String, so it suffices
            // just to split the string just after the match. In this case,
            // however, life is harder, because the buffer is not a string but a
            // collection of cells containing `char`s.
            //
            // To "consume" the buffer, overwrite everything up through the
            // match string with spaces. Start by truncating the current buffer
            // contents down to the substring that ends in the match.
            let last_byte = idx + waiter.wanted.len();
            contents.truncate(last_byte);

            // Then move the cursor to the top left and "type" as many blank
            // spaces as there were characters in the buffer. Note that termwiz
            // inserts extra '\n' characters at the end of every line that need
            // to be ignored.
            let char_count = contents.chars().filter(|c| *c != '\n').count();

            // Before typing anything, remember the old cursor position so that
            // it can be restored after typing the spaces.
            //
            // It's insufficient to assume that the last character of the match
            // was actually "typed" such that the cursor advanced past it. For
            // example, if a match string ends with "$ ", and the guest moves to
            // the start of an empty line and types a single "$", the cursor is
            // in column 1 (just past the "$"), but the match is satisfied by
            // the "pre-existing" space in that column.
            let (old_col, old_row) = self.surface.cursor_position();
            self.surface.add_change(make_absolute_cursor_position(0, 0));
            self.surface.add_change(Change::Text(" ".repeat(char_count)));
            let seq = self
                .surface
                .add_change(make_absolute_cursor_position(old_col, old_row));
            self.surface.flush_changes_older_than(seq);

            // Remove the match string from the match contents and push
            // everything else back to the listener.
            contents.truncate(idx);
            let _ = waiter.preceding_tx.send(contents);
        } else {
            self.waiter = Some(waiter);
        }
    }
}

impl Buffer for Vt80x24 {
    fn process_bytes(&mut self, bytes: &[u8]) {
        let actions = self.parser.parse_as_vec(bytes);
        self.apply_actions(&actions);
    }

    fn register_wait_for_output(&mut self, waiter: OutputWaiter) {
        self.satisfy_or_set_wait(waiter);
    }

    fn cancel_wait_for_output(&mut self) -> Option<OutputWaiter> {
        self.waiter.take()
    }
}

/// Provides shorthand to create a termwiz `CursorPosition` from zero-based
/// column and row indices.
fn make_absolute_cursor_position(col: usize, row: usize) -> Change {
    Change::CursorPosition {
        x: Position::Absolute(col),
        y: Position::Absolute(row),
    }
}
