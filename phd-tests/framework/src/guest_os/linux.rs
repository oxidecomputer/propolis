// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Helper functions for building guest OS adaptations for Linux OSes.

use super::{CommandSequence, CommandSequenceEntry, GuestOs};

/// Yields an `stty` command that tells the guest terminal to behave as though
/// it is 9,999 columns wide.
pub(super) fn stty_enable_long_lines<'a>(
    guest_os: &impl GuestOs,
) -> CommandSequence<'a> {
    CommandSequence(vec![
        CommandSequenceEntry::write_str("stty -F `tty` cols 9999"),
        CommandSequenceEntry::wait_for(guest_os.get_shell_prompt()),
    ])
}
