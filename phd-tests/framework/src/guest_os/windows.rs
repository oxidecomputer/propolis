// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Helper functions for generating Windows guest OS adaptations.

use super::{CommandSequence, CommandSequenceEntry, GuestOsKind};

/// Emits the login seqeunce for the given `guest`, which must be one of the
/// Windows guest OS flavors.
///
/// This login sequence assumes the following:
///
/// - The image has been generalized (by running `sysprep /generalize`) and is
///   configured so that on first boot it will skip the out-of-box experience
///   (OOBE) and initialize the local administrator account with the appropriate
///   password. See [MSDN's Windows Setup
///   documentation](https://learn.microsoft.com/en-us/windows-hardware/manufacture/desktop/generalize?view=windows-11)
///   for more details.
/// - Cygwin is installed to C:\cygwin and can be launched by invoking
///   C:\cygwin\cygwin.bat.
/// - The local administrator account is enabled with password `0xide#1Fan`.
pub(super) fn get_login_sequence_for(guest: GuestOsKind) -> CommandSequence {
    assert!(matches!(
        guest,
        GuestOsKind::WindowsServer2019 | GuestOsKind::WindowsServer2022
    ));

    let mut commands = vec![
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
    ];

    // Windows Server 2019's serial console-based command prompts default to
    // trying to drive a VT100 terminal themselves instead of emitting
    // characters and letting the recipient display them in whatever style it
    // likes. This only happens once the command prompt has been activated, so
    // only switch buffering modes after entering credentials.
    if let GuestOsKind::WindowsServer2019 = guest {
        commands.extend([
            CommandSequenceEntry::ChangeSerialConsoleBuffer(
                crate::serial::BufferKind::Vt80x24,
            ),
            // Server 2019 also likes to debounce keystrokes, so set a small
            // delay between characters to try to avoid this. (This value was
            // chosen by experimentation; there doesn't seem to be a guest
            // setting that controls this interval.)
            CommandSequenceEntry::SetSerialByteWriteDelay(
                std::time::Duration::from_millis(125),
            ),
        ]);
    }

    commands.extend([
        // For reasons unknown, the first command prompt the serial console
        // produces is flaky when being sent actual commands (it appears to
        // eat the command and just process the newline). It also appears to
        // prefer carriage returns to linefeeds. Accommodate this behavior
        // until Cygwin is launched.
        CommandSequenceEntry::WaitFor("C:\\Windows\\system32>"),
        CommandSequenceEntry::WriteStr("cls\r"),
        CommandSequenceEntry::WaitFor("C:\\Windows\\system32>"),
        CommandSequenceEntry::WriteStr("C:\\cygwin\\cygwin.bat\r"),
        CommandSequenceEntry::WaitFor("$ "),
        // Tweak the command prompt so that it appears on a single line with
        // no leading newlines.
        CommandSequenceEntry::WriteStr("PS1='\\u@\\h:$ '"),
    ]);

    CommandSequence(commands)
}
