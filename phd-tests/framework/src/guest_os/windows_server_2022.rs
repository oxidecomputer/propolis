// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Guest OS adaptations for Windows Server 2022 images. See [the general
//! Windows module](mod@super::windows) documentation for more information.

use super::{CommandSequence, GuestOs, GuestOsKind};

/// The guest adapter for Windows Server 2022 images. See [the general
/// Windows module](mod@super::windows) documentation for more information about
/// the configuration this adapter requires.
pub(super) struct WindowsServer2022;

impl GuestOs for WindowsServer2022 {
    fn get_login_sequence(&self) -> CommandSequence {
        super::windows::get_login_sequence_for(GuestOsKind::WindowsServer2022)
    }

    fn get_shell_prompt(&self) -> &'static str {
        "$ "
    }

    fn read_only_fs(&self) -> bool {
        false
    }

    fn graceful_reboot(&self) -> CommandSequence {
        self.shell_command_sequence("shutdown /r /t 0 /d p:0:0")
    }
}
