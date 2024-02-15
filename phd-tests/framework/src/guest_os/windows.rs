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
/// - Cygwin is installed to C:\cygwin and can be launched by invoking
///   C:\cygwin\cygwin.bat.
/// - The local administrator account is enabled with password `0xide#1Fan`.
pub(super) fn get_login_sequence_for<'a>(
    guest: GuestOsKind,
) -> CommandSequence<'a> {
    assert!(matches!(
        guest,
        GuestOsKind::WindowsServer2016
            | GuestOsKind::WindowsServer2019
            | GuestOsKind::WindowsServer2022
    ));

    let mut commands = vec![
        CommandSequenceEntry::WaitFor(
            "Computer is booting, SAC started and initialized.".into(),
        ),
        CommandSequenceEntry::WaitFor(
            "EVENT: The CMD command is now available.".into(),
        ),
        CommandSequenceEntry::WaitFor("SAC>".into()),
        CommandSequenceEntry::WriteStr("cmd".into()),
        CommandSequenceEntry::WaitFor("Channel: Cmd0001".into()),
        CommandSequenceEntry::WaitFor("SAC>".into()),
        CommandSequenceEntry::WriteStr("ch -sn Cmd0001".into()),
        CommandSequenceEntry::WaitFor(
            "Use any other key to view this channel.".into(),
        ),
        CommandSequenceEntry::WriteStr("".into()),
        CommandSequenceEntry::WaitFor("Username:".into()),
        CommandSequenceEntry::WriteStr("Administrator".into()),
        CommandSequenceEntry::WaitFor("Domain  :".into()),
        CommandSequenceEntry::WriteStr("".into()),
        CommandSequenceEntry::WaitFor("Password:".into()),
        CommandSequenceEntry::WriteStr("0xide#1Fan".into()),
    ];

    // Earlier Windows Server versions' serial console-based command prompts
    // default to trying to drive a VT100 terminal themselves instead of
    // emitting characters and letting the recipient display them in whatever
    // style it likes. This only happens once the command prompt has been
    // activated, so only switch buffering modes after entering credentials.
    if matches!(
        guest,
        GuestOsKind::WindowsServer2016 | GuestOsKind::WindowsServer2019
    ) {
        commands.extend([
            CommandSequenceEntry::ChangeSerialConsoleBuffer(
                crate::serial::BufferKind::Vt80x24,
            ),
            // These versions also like to debounce keystrokes, so set a delay
            // between repeated characters to try to avoid this. This is a very
            // conservative delay to try to avoid test flakiness; fortunately,
            // it only applies when typing the same character multiple times in
            // a row.
            CommandSequenceEntry::SetRepeatedCharacterDebounce(
                std::time::Duration::from_secs(1),
            ),
        ]);
    }

    commands.extend([
        // For reasons unknown, the first command prompt the serial console
        // produces is flaky when being sent actual commands (it appears to
        // eat the command and just process the newline). It also appears to
        // prefer carriage returns to linefeeds. Accommodate this behavior
        // until Cygwin is launched.
        CommandSequenceEntry::WaitFor("C:\\Windows\\system32>".into()),
        CommandSequenceEntry::WriteStr("cls\r".into()),
        CommandSequenceEntry::WaitFor("C:\\Windows\\system32>".into()),
        CommandSequenceEntry::WriteStr("C:\\cygwin\\cygwin.bat\r".into()),
        CommandSequenceEntry::WaitFor("$ ".into()),
        // Tweak the command prompt so that it appears on a single line with
        // no leading newlines.
        CommandSequenceEntry::WriteStr("PS1='\\u@\\h:$ '".into()),
    ]);

    CommandSequence(commands)
}
