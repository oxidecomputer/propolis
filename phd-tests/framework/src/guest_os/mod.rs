// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Traits and objects that abstract over differences between guest OS
//! distributions.

use std::{borrow::Cow, str::FromStr};

use serde::{Deserialize, Serialize};

mod alpine;
mod debian11_nocloud;
mod ubuntu22_04;
mod windows;
mod windows_server_2019;
mod windows_server_2022;

/// An entry in a sequence of interactions with the guest's command prompt.
#[derive(Debug)]
pub(super) enum CommandSequenceEntry {
    /// Wait for the supplied string to appear on the guest serial console.
    WaitFor(&'static str),

    /// Write the specified string as a command to the guest serial console.
    WriteStr(&'static str),

    /// Change the serial console buffering discipline to the supplied
    /// discipline.
    ChangeSerialConsoleBuffer(crate::serial::BufferKind),

    /// Set a delay between writing individual bytes to the guest serial console
    /// to avoid keyboard debouncing logic in guests.
    SetSerialByteWriteDelay(std::time::Duration),
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

    /// Some guests need to amend incoming shell commands from tests in order to
    /// get output to display on the serial console in a way those guests can
    /// accept (e.g. by clearing the screen immediately before running each
    /// command). This function amends an incoming command according to the
    /// guest adapter's instructions.
    fn amend_shell_command<'a>(&self, cmd: &'a str) -> Cow<'a, str> {
        Cow::Borrowed(cmd)
    }
}

#[allow(dead_code)]
#[derive(Clone, Copy, Debug, Serialize, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum GuestOsKind {
    Alpine,
    Debian11NoCloud,
    Ubuntu2204,
    WindowsServer2019,
    WindowsServer2022,
}

impl FromStr for GuestOsKind {
    type Err = std::io::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "alpine" => Ok(Self::Alpine),
            "debian11nocloud" => Ok(Self::Debian11NoCloud),
            "ubuntu2204" => Ok(Self::Ubuntu2204),
            "windowsserver2022" => Ok(Self::WindowsServer2022),
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
        GuestOsKind::WindowsServer2019 => {
            Box::new(windows_server_2019::WindowsServer2019)
        }
        GuestOsKind::WindowsServer2022 => {
            Box::new(windows_server_2022::WindowsServer2022)
        }
    }
}
