// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Guest OS adaptations for Debian 11 nocloud images.

use super::{CommandSequence, CommandSequenceEntry, GuestOs};

pub(super) struct Debian11NoCloud;

impl GuestOs for Debian11NoCloud {
    fn get_login_sequence(&self) -> CommandSequence {
        CommandSequence(vec![
            CommandSequenceEntry::wait_for("debian login: "),
            CommandSequenceEntry::write_str("root"),
            CommandSequenceEntry::wait_for(self.get_shell_prompt()),
        ])
        .extend(super::linux::stty_enable_long_lines(self))
    }

    fn get_shell_prompt(&self) -> &'static str {
        "root@debian:~#"
    }

    fn read_only_fs(&self) -> bool {
        false
    }

    fn graceful_reboot(&self) -> CommandSequence {
        // On Debian 11, `reboot` does not seem to be the same wrapper for
        // `systemctl reboot` as it is on more recent Ubuntu. Whatever it *is*,
        // it does its job before a new prompt line is printed, so we can only
        // wait to see a new login sequence.
        //
        // While `systemctl reboot` does exist here, and is mechanically more
        // like Ubuntu's `reboot`, just using `reboot` on Debian gets the job
        // done and keeps our instructions consistent across Linuxes.
        self.shell_command_sequence("reboot")
    }
}
