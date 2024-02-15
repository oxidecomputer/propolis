// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Guest OS adaptations for Debian 11 nocloud images.

use super::{CommandSequence, CommandSequenceEntry, GuestOs};

pub(super) struct Debian11NoCloud;

impl GuestOs for Debian11NoCloud {
    fn get_login_sequence(&self) -> CommandSequence {
        CommandSequence(vec![
            CommandSequenceEntry::WaitFor("debian login: ".into()),
            CommandSequenceEntry::WriteStr("root".into()),
            CommandSequenceEntry::WaitFor(self.get_shell_prompt().into()),
        ])
    }

    fn get_shell_prompt(&self) -> &'static str {
        "root@debian:~#"
    }

    fn read_only_fs(&self) -> bool {
        false
    }
}
