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

const SURFACE_ROWS: usize = 24;

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

        if tracing::enabled!(tracing::Level::TRACE) {
            let contents = self.surface.screen_chars_to_string();
            trace_buffer_contents(&contents);
        }

        if let Some(waiter) = self.waiter.take() {
            self.satisfy_or_set_wait(waiter);
        }
    }

    /// Attempts to satisfy the wait described by `waiter`. If the wait is not
    /// immediately satisfiable, stores `waiter` to try again later.
    fn satisfy_or_set_wait(&mut self, waiter: OutputWaiter) {
        assert!(self.waiter.is_none());

        let _too_long = waiter.wanted.lines().find(|line| line.len() > 80);
        assert_eq!(
            _too_long, None,
            "vt80x24 waits for lines of more than 80 characters will never be \
            satisfied"
        );

        let mut contents = self.surface.screen_chars_to_string();
        if let Some(idx) = contents.rfind(&waiter.wanted) {
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

    fn clear(&mut self) {
        let cursor_pos = self.surface.cursor_position();
        let seq = self.surface.add_changes(vec![
            Change::ClearScreen(ColorAttribute::Default),
            make_absolute_cursor_position(cursor_pos.0, cursor_pos.1),
        ]);
        self.surface.flush_changes_older_than(seq);
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

fn trace_buffer_contents(contents: &str) {
    // Find the index of the last line in the buffer that isn't blank.
    let last_non_empty = contents
        .lines()
        .rev()
        .position(|l| l.chars().any(|c| c != ' '))
        .map(|pos| SURFACE_ROWS - pos - 1);

    if let Some(last_non_empty) = last_non_empty {
        for (idx, line) in contents.lines().enumerate() {
            if idx > last_non_empty {
                break;
            }
            trace!(idx, line, "termwiz buffer contents");
        }
    } else {
        trace!("termwiz buffer is empty");
    }
}
