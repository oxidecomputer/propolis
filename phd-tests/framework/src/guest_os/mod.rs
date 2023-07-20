//! Traits and objects that abstract over differences between guest OS
//! distributions.

use std::str::FromStr;

use serde::{Deserialize, Serialize};

mod alpine;
mod debian11_nocloud;
mod ubuntu22_04;

/// An entry in a sequence of interactions with the guest's command prompt.
pub(super) enum CommandSequenceEntry {
    /// Wait for the supplied string to appear on the guest serial console.
    WaitFor(&'static str),

    /// Write the specified string as a command to the guest serial console.
    WriteStr(&'static str),
}

pub(super) struct CommandSequence(pub Vec<CommandSequenceEntry>);

pub(super) trait GuestOs {
    /// Retrieves the command sequence used to wait for the OS to boot and log
    /// into it.
    fn get_login_sequence(&self) -> CommandSequence;

    /// Retrieves the default shell prompt for this OS.
    fn get_shell_prompt(&self) -> &'static str;

    /// Indicates whether the guest has a read-only filesystem.
    fn read_only_fs(&self) -> bool;
}

#[allow(dead_code)]
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GuestOsKind {
    Alpine,
    Debian11NoCloud,
    Ubuntu2204,
}

impl FromStr for GuestOsKind {
    type Err = std::io::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "alpine" => Ok(Self::Alpine),
            "debian11nocloud" => Ok(Self::Debian11NoCloud),
            "ubuntu2204" => Ok(Self::Ubuntu2204),
            _ => Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("Unrecognized guest OS kind {}", s),
            )),
        }
    }
}

pub(super) fn get_guest_os_adapter(kind: GuestOsKind) -> Box<dyn GuestOs> {
    match kind {
        GuestOsKind::Alpine => Box::new(alpine::Alpine),
        GuestOsKind::Debian11NoCloud => {
            Box::new(debian11_nocloud::Debian11NoCloud)
        }
        GuestOsKind::Ubuntu2204 => Box::new(ubuntu22_04::Ubuntu2204),
    }
}
