// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Functionality common to all Windows guests.

use crate::TestVm;

use super::{CommandSequence, CommandSequenceEntry, GuestOsKind};

use tracing::info;

/// A wrapper that provides Windows-specific extensions to the core `TestVm`
/// implementation.
pub struct WindowsVm<'a> {
    /// The VM being extended by this structure. The framework is required to
    /// ensure that the VM is actually configured to run a Windows guest OS.
    pub(crate) vm: &'a TestVm,
}

impl WindowsVm<'_> {
    /// Runs `cmd` as a Powershell command.
    pub async fn run_powershell_command(
        &self,
        cmd: &str,
    ) -> anyhow::Result<String> {
        assert!(self.vm.guest_os_kind().is_windows());

        info!(cmd, "executing Powershell command");

        // Use Powershell's -encodedCommand switch to keep important Powershell
        // sigils in the command (like "$") from being interpreted by whatever
        // shell is being used to invoke Powershell. This switch expects that
        // the encoded string will decode into a UTF-16 string; `str`s are, of
        // course, UTF-8, so switch encodings before converting to base64.
        let utf16 = cmd.encode_utf16().collect::<Vec<u16>>();
        let base64 = base64::Engine::encode(
            &base64::engine::general_purpose::STANDARD,
            unsafe { utf16.align_to::<u8>().1 },
        );

        let cmd = format!("powershell -encodedCommand {base64}");
        self.vm.run_shell_command(&cmd).await
    }
}

impl std::ops::Deref for WindowsVm<'_> {
    type Target = TestVm;

    fn deref(&self) -> &Self::Target {
        self.vm
    }
}

const CYGWIN_CMD: &str = "C:\\cygwin\\cygwin.bat\r";

/// Prepends a `reset` command to the shell command supplied in `cmd`. Windows
/// versions that drive a VT100 terminal can use this to try to force Windows to
/// clear and redraw the entire screen before displaying the command's output.
/// Without this, Windows may not render the post-output command prompt if the
/// post-command terminal state happens to place a prompt at a location that
/// already had onen pre-command.
pub(super) fn prepend_reset_to_shell_command(cmd: &str) -> String {
    format!("reset && {cmd}")
}

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
        // Look for `BdsDxe:` as a sign that we're actually seeing a fresh boot.
        // This is not terribly important in the case of a first boot, but in a
        // case such as logging out and waiting for reboot, exiting a cmd.exe
        // session causes Windows to redraw its previous screen - everything
        // past `Computer is booting, ...` below.
        //
        // A test that tries to boot and wait for a new login sequence would
        // then incorrectly identify the already-booted VM as the freshly-booted
        // OS it was waiting for, log in again, and at some point later finally
        // actually reboot.
        //
        // At least on Windows Server 2022, there is an XML prelude that is
        // printed to COM1 that we could look for here, but check for `BdsDxe: `
        // instead as that comes from OVMF and will be consistent regardless of
        // guest OS version.
        CommandSequenceEntry::wait_for("BdsDxe: loading "),
        CommandSequenceEntry::wait_for(
            "Computer is booting, SAC started and initialized.",
        ),
        CommandSequenceEntry::wait_for(
            "EVENT: The CMD command is now available.",
        ),
        CommandSequenceEntry::wait_for("SAC>"),
        CommandSequenceEntry::write_str("cmd"),
        CommandSequenceEntry::wait_for("Channel: Cmd0001"),
        CommandSequenceEntry::wait_for("SAC>"),
        CommandSequenceEntry::write_str("ch -sn Cmd0001"),
        CommandSequenceEntry::wait_for(
            "Use any other key to view this channel.",
        ),
        CommandSequenceEntry::write_str(""),
        CommandSequenceEntry::wait_for("C:\\Windows\\system32>"),
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
                std::time::Duration::from_millis(1500),
            ),
        ]);
    }

    commands.extend([
        // There appears (from observing Windows test reliability) to be some
        // kind of race at command prompt startup that can cause characters to
        // be eaten if they're typed too quickly after the command prompt
        // session launches. To get around this, try to send "serial console ok"
        // strings until one of them gets echoed back correctly or the entire
        // boot times out.
        CommandSequenceEntry::wait_for("C:\\Windows\\system32>"),
        CommandSequenceEntry::EstablishConsistentEcho {
            send: "echo serial console ok\\n\r\n".into(),
            expect: "serial console ok".into(),
            timeout: std::time::Duration::from_millis(250),
        },
        // Make sure there's a clean command prompt after establishing the echo.
        CommandSequenceEntry::write_str("cls\r"),
        CommandSequenceEntry::wait_for("C:\\Windows\\system32>"),
    ]);

    // Keep Cygwin from wrapping lines unexpectedly on Windows Server 2022 by
    // maximizing the effective console size before launching Cygwin. This just
    // confuses matters on Server 2016 and 2019, so on those guests just launch
    // Cygwin directly.
    if let GuestOsKind::WindowsServer2022 = guest {
        commands.push(CommandSequenceEntry::write_str(format!(
            "mode con cols=9999 lines=9999 && {CYGWIN_CMD}",
        )));
    } else {
        commands.push(CommandSequenceEntry::write_str(CYGWIN_CMD));
    }

    commands.extend([
        CommandSequenceEntry::wait_for("$ "),
        // Tweak the command prompt so that it appears on a single line with
        // no leading newlines.
        CommandSequenceEntry::write_str("PS1='$ '"),
    ]);

    CommandSequence(commands)
}
