//! Guest OS adaptations for Debian 11 nocloud images.

use super::{CommandSequence, CommandSequenceEntry, GuestOs};

pub(super) struct Debian11NoCloud;

impl GuestOs for Debian11NoCloud {
    fn get_login_sequence(&self) -> CommandSequence {
        CommandSequence(vec![
            CommandSequenceEntry::WaitFor("debian login: "),
            CommandSequenceEntry::WriteStr("root"),
            CommandSequenceEntry::WaitFor(self.get_shell_prompt()),
        ])
    }

    fn get_shell_prompt(&self) -> &'static str {
        "root@debian:~#"
    }
}
