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

    fn amend_shell_command<'a>(&self, cmd: &'a str) -> Cow<'a, str> {
        // Ensure that after executing a shell command, the 80x24 serial console
        // buffer contains only the output of the command.
        //
        // `reset` is used to try to force the guest to clear and redraw the
        // entire terminal so that there is no "left over" command prompt in the
        // guest's terminal buffer. This ensures that the guest will print the
        // new prompt instead of eliding it because it believes it's already on
        // the screen.
        Cow::from(format!("reset; {}", cmd))
    }
}
