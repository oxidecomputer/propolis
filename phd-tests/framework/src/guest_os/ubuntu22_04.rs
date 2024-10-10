// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Guest OS adaptations for Ubuntu 22.04 images. These must be prepped with
//! a cloud-init disk that is configured with the appropriate user and password.

use super::{CommandSequence, CommandSequenceEntry, GuestOs};

pub(super) struct Ubuntu2204;

impl GuestOs for Ubuntu2204 {
    fn get_login_sequence(&self) -> CommandSequence {
        CommandSequence(vec![
            CommandSequenceEntry::wait_for("ubuntu login: "),
            CommandSequenceEntry::write_str("ubuntu"),
            CommandSequenceEntry::wait_for("Password: "),
            CommandSequenceEntry::write_str("1!Passw0rd"),
            CommandSequenceEntry::wait_for(self.get_shell_prompt()),
        ])
    }

    fn get_shell_prompt(&self) -> &'static str {
        "ubuntu@ubuntu:~$"
    }

    fn read_only_fs(&self) -> bool {
        false
    }

    fn graceful_reboot(&self) -> CommandSequence {
        // Ubuntu `reboot` seems to be mechanically similar to Alpine `reboot`,
        // except mediated by SystemD rather than OpenRC. We'll get a new shell
        // prompt, and then the system reboots shortly after. Just issuing
        // `reboot` and waiting for a login prompt is the lowest common
        // denominator across Linuxes.
        self.shell_command_sequence("reboot")
    }
}
