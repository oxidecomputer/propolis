// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Guest OS adaptations for Windows Server 2019 images. See [the general
//! Windows module](mod@super::windows) documentation for more information.

use std::borrow::Cow;

use super::{CommandSequence, GuestOs, GuestOsKind};

/// The guest adapter for Windows Server 2019 images. See [the general
/// Windows module](mod@super::windows) documentation for more information about
/// the configuration this adapter requires.
pub(super) struct WindowsServer2019;

impl GuestOs for WindowsServer2019 {
    fn get_login_sequence(&self) -> CommandSequence {
        super::windows::get_login_sequence_for(GuestOsKind::WindowsServer2019)
    }

    fn get_shell_prompt(&self) -> &'static str {
        "Administrator@PHD-WINDOWS:$ "
    }

    fn read_only_fs(&self) -> bool {
        false
    }

    fn amend_shell_command<'a>(&self, cmd: &'a str) -> Cow<'a, str> {
        // The simplest way to ensure that the 80x24 terminal buffer contains
        // just the output of the most recent command and the subsequent prompt
        // is to ask Windows to clear the screen and run the command in a single
        // statement.
        //
        // Use Cygwin bash's `reset` instead of `clear` or `cls` to try to force
        // Windows to clear and redraw the entire terminal before displaying any
        // command output. Without this, Windows sometimes reprints a new
        // command prompt to its internal screen buffer before re-rendering
        // anything to the terminal; when this happens, it doesn't re-send the
        // new command prompt, since it's "already there" on the output terminal
        // (even though it may have been cleared from the match buffer).
        Cow::from(format!("reset; {}", cmd))
    }
}
