// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Common helper functions for issuing shell commands to guests and handling
//! their outputs.

use std::borrow::Cow;

use super::{CommandSequence, CommandSequenceEntry};

/// Produces the shell command sequence necessary to execute `cmd` in a guest's
/// shell, given that the guest is using the supplied serial console buffering
/// discipline.
///
/// This routine assumes that multi-line commands will be echoed with `> ` at
/// the start of each line in the command. This is technically shell-dependent
/// but is true for all the shell types in PHD's currently-supported guests.
pub(super) fn shell_command_sequence(
    cmd: Cow<'_, str>,
    buffer_kind: crate::serial::BufferKind,
) -> CommandSequence {
    let echo = cmd.trim_end().replace('\n', "\n> ");
    match buffer_kind {
        crate::serial::BufferKind::Raw => CommandSequence(vec![
            CommandSequenceEntry::write_str(cmd),
            CommandSequenceEntry::wait_for(echo),
            CommandSequenceEntry::ClearBuffer,
            CommandSequenceEntry::write_str("\n"),
        ]),

        crate::serial::BufferKind::Vt80x24 => {
            // In 80x24 mode, it's simplest to issue multi-line operations one
            // line at a time and wait for each line to be echoed before
            // starting the next. For very long commands (more than 24 lines),
            // this avoids having to deal with lines scrolling off the buffer
            // before they can be waited for.
            let cmd_lines = cmd.trim_end().lines();
            let echo_lines = echo.lines();
            let mut seq = vec![];

            let mut iter = cmd_lines.zip(echo_lines).peekable();
            while let Some((cmd, echo)) = iter.next() {
                seq.push(CommandSequenceEntry::write_str(cmd.to_owned()));
                seq.push(CommandSequenceEntry::wait_for(echo.to_owned()));

                if iter.peek().is_some() {
                    seq.push(CommandSequenceEntry::write_str("\n"));
                }
            }

            // Before issuing the command, clear any stale echoed characters
            // from the serial console buffer. This ensures that the next prompt
            // is preceded in the buffer only by the output of the issued
            // command.
            seq.push(CommandSequenceEntry::ClearBuffer);
            seq.push(CommandSequenceEntry::write_str("\n"));
            CommandSequence(seq)
        }
    }
}
