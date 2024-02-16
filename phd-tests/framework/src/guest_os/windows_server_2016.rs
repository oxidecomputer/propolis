// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Guest OS adaptations for Windows Server 2016 images. See [the general
//! Windows module](mod@super::windows) documentation for more information.

use std::borrow::Cow;

use super::{CommandSequence, GuestOs, GuestOsKind};

/// The guest adapter for Windows Server 2016 images. See [the general
/// Windows module](mod@super::windows) documentation for more information about
/// the configuration this adapter requires.
pub(super) struct WindowsServer2016;

impl GuestOs for WindowsServer2016 {
    fn get_login_sequence(&self) -> CommandSequence {
        super::windows::get_login_sequence_for(GuestOsKind::WindowsServer2016)
    }

    fn get_shell_prompt(&self) -> &'static str {
        "Administrator@PHD-WINDOWS:$ "
    }

    fn read_only_fs(&self) -> bool {
        false
    }

    fn shell_command_sequence<'a>(&self, cmd: &'a str) -> CommandSequence<'a> {
        // `reset` the command prompt before issuing the command to try to force
        // Windows to redraw the subsequent command prompt. Without this,
        // Windows may not draw the prompt if the post-command state happens to
        // place a prompt at a location that already had one pre-command.
        let cmd = format!("reset && {cmd}");
        super::shell_commands::shell_command_sequence(
            Cow::Owned(cmd),
            crate::serial::BufferKind::Vt80x24,
        )
    }
}
