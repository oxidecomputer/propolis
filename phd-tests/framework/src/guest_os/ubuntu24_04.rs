// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Guest OS adaptations for Ubuntu 24.04 images. These must be prepped with
//! a cloud-init disk that is configured with the appropriate user and password.

use super::{CommandSequence, CommandSequenceEntry, GuestOs};

pub(super) struct Ubuntu2404;

impl GuestOs for Ubuntu2404 {
    fn get_login_sequence(&self) -> CommandSequence {
        CommandSequence(vec![
            CommandSequenceEntry::wait_for("ubuntu login: "),
            CommandSequenceEntry::write_str("root"),
            CommandSequenceEntry::wait_for(self.get_shell_prompt()),
        ])
    }

    fn get_shell_prompt(&self) -> &'static str {
        "root@ubuntu:~#"
    }

    fn read_only_fs(&self) -> bool {
        false
    }
}
