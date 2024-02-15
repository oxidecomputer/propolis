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
pub(super) fn shell_command_sequence<'a>(
    cmd: Cow<'a, str>,
    buffer_kind: crate::serial::BufferKind,
) -> CommandSequence {
    let echo = cmd.trim_end().replace('\n', "\n> ");
    match buffer_kind {
        crate::serial::BufferKind::Raw => CommandSequence(vec![
            CommandSequenceEntry::WriteStr(cmd.into()),
            CommandSequenceEntry::WaitFor(echo.into()),
            CommandSequenceEntry::WriteStr("\n".into()),
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
            for (cmd, echo) in cmd_lines.zip(echo_lines) {
                seq.push(CommandSequenceEntry::WriteStr(cmd.to_owned().into()));
                seq.push(CommandSequenceEntry::WaitFor(echo.to_owned().into()));
                seq.push(CommandSequenceEntry::WriteStr("\n".into()));
            }

            CommandSequence(seq)
        }
    }
}
