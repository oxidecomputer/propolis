// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Guest OS adaptations for Windows Server 2022 images. This adapter assumes
//! the guest OS image has been generalized, that Cygwin has been installed, and
//! that the local administrator user was configured at image generation time to
//! have the appropriate password.

use super::{CommandSequence, CommandSequenceEntry, GuestOs};

/// Provides a guest OS adapter for Windows Server 2022 images. This adapter
/// assumes the following:
///
/// - The image has been generalized (by running `sysprep /generalize`) and is
///   configured so that on first boot it will skip the out-of-box experience
///   (OOBE) and initialize the local administrator account with the appropriate
///   password. See [MSDN's Windows Setup
///   documentation](https://learn.microsoft.com/en-us/windows-hardware/manufacture/desktop/generalize?view=windows-11)
///   for more details.
/// - Cygwin is installed to C:\cygwin and can be launched by invoking
///   C:\cygwin\cygwin.bat.
pub(super) struct WindowsServer2022;

impl GuestOs for WindowsServer2022 {
    fn get_login_sequence(&self) -> CommandSequence {
        CommandSequence(vec![
            // Assume the image will need to reboot one last time after being
            // specialized.
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
