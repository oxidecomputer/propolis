// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Guest OS adaptations for Alpine Linux's "virtual" image.

use super::{CommandSequence, CommandSequenceEntry, GuestOs};

pub(super) struct Alpine;

impl GuestOs for Alpine {
    fn get_login_sequence(&self) -> CommandSequence {
        CommandSequence(vec![
            CommandSequenceEntry::WaitFor("localhost login: "),
            CommandSequenceEntry::WriteStr("root"),
            CommandSequenceEntry::WaitFor(self.get_shell_prompt()),
        ])
    }

    fn get_shell_prompt(&self) -> &'static str {
        "localhost:~#"
    }

    fn read_only_fs(&self) -> bool {
        true
    }
}
