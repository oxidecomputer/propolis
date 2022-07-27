//! Guest OS adaptations for Alpine Linux's "virtual" image.

use super::{CommandSequence, CommandSequenceEntry, GuestOs};

pub(super) struct Alpine;

impl GuestOs for Alpine {
    fn get_login_sequence(&self) -> CommandSequence {
        CommandSequence(vec![
            CommandSequenceEntry::WaitFor("localhost login: "),
            CommandSequenceEntry::WriteStr("root"),
            CommandSequenceEntry::WaitFor(self.get_shell_prompt()),
        ])
    }

    fn get_shell_prompt(&self) -> &'static str {
        "localhost:~#"
    }
}
