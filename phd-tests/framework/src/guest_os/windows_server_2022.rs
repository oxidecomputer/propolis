// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Guest OS adaptations for Windows Server 2022 images. This adapter assumes
//! the guest OS image has been generalized, that Cygwin has been installed, and
//! that the local administrator user was configured at image generation time to
//! have the appropriate password.

use super::{CommandSequence, CommandSequenceEntry, GuestOs};

pub(super) struct WindowsServer2022;

impl GuestOs for WindowsServer2022 {
    fn get_login_sequence(&self) -> CommandSequence {
        CommandSequence(vec![
            // The image is generalized and needs to be specialized. Let it boot
            // once, then wait for it to boot again and only then start waiting
            // for the command prompt to become available.
            CommandSequenceEntry::WaitFor(
                "Computer is booting, SAC started and initialized.",
            ),
            CommandSequenceEntry::WaitFor(
                "Computer is booting, SAC started and initialized.",
            ),
            CommandSequenceEntry::WaitFor(
                "EVENT: The CMD command is now available.",
            ),
            CommandSequenceEntry::WaitFor("SAC>"),
            CommandSequenceEntry::WriteStr("cmd"),
            CommandSequenceEntry::WaitFor("Channel: Cmd0001"),
            CommandSequenceEntry::WaitFor("SAC>"),
            CommandSequenceEntry::WriteStr("ch -sn Cmd0001"),
            CommandSequenceEntry::WaitFor(
                "Use any other key to view this channel.",
            ),
            CommandSequenceEntry::WriteStr(""),
            CommandSequenceEntry::WaitFor("Username:"),
            CommandSequenceEntry::WriteStr("Administrator"),
            CommandSequenceEntry::WaitFor("Domain  :"),
            CommandSequenceEntry::WriteStr(""),
            CommandSequenceEntry::WaitFor("Password:"),
            CommandSequenceEntry::WriteStr("0xide#1Fan"),
            // For reasons unknown, the first command prompt the serial console
            // produces is flaky when being sent actual commands (it appears to
            // eat the command and just process the newline). It also appears to
            // prefer carriage returns to linefeeds. Accommodate this behavior
            // until Cygwin is launched.
            CommandSequenceEntry::WaitFor("C:\\Windows\\system32>"),
            CommandSequenceEntry::WriteStr("\r"),
            CommandSequenceEntry::WaitFor("C:\\Windows\\system32>"),
            CommandSequenceEntry::WriteStr("C:\\cygwin\\cygwin.bat\r"),
            CommandSequenceEntry::WaitFor("$ "),
            // Tweak the command prompt so that it appears on a single line with
            // no leading newlines.
            CommandSequenceEntry::WriteStr("PS1='\\u@\\h:$ '"),
        ])
    }

    fn get_shell_prompt(&self) -> &'static str {
        "Administrator@PHD-WINDOWS:$ "
    }

    fn read_only_fs(&self) -> bool {
        false
    }
}
