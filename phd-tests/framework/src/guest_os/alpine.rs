// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Guest OS adaptations for Alpine Linux's "virtual" image.

use super::{CommandSequence, CommandSequenceEntry, GuestOs};

pub(super) struct Alpine;

impl GuestOs for Alpine {
    fn get_login_sequence(&self) -> CommandSequence {
        CommandSequence(vec![
            CommandSequenceEntry::wait_for("localhost login: "),
            CommandSequenceEntry::write_str("root"),
            CommandSequenceEntry::wait_for(self.get_shell_prompt()),
        ])
    }

    fn get_shell_prompt(&self) -> &'static str {
        "localhost:~#"
    }

    fn read_only_fs(&self) -> bool {
        true
    }

    fn shell_command_sequence<'a>(&self, cmd: &'a str) -> CommandSequence<'a> {
        super::shell_commands::shell_command_sequence(
            std::borrow::Cow::Borrowed(cmd),
            crate::serial::BufferKind::Raw,
        )
    }
}
