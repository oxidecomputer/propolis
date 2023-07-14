//! Guest OS adaptations for Ubuntu 22.04 images. These must be prepped with
//! a cloud-init disk that is configured with the appropriate user and password.

use super::{CommandSequence, CommandSequenceEntry, GuestOs};

pub(super) struct Ubuntu2204;

impl GuestOs for Ubuntu2204 {
    fn get_login_sequence(&self) -> CommandSequence {
        CommandSequence(vec![
            CommandSequenceEntry::WaitFor("ubuntu login: "),
            CommandSequenceEntry::WriteStr("ubuntu"),
            CommandSequenceEntry::WaitFor("Password: "),
            CommandSequenceEntry::WriteStr("1!Passw0rd"),
            CommandSequenceEntry::WaitFor(self.get_shell_prompt()),
        ])
    }

    fn get_shell_prompt(&self) -> &'static str {
        "ubuntu@ubuntu:~$"
    }

    fn read_only_fs(&self) -> bool {
        false
    }
}
