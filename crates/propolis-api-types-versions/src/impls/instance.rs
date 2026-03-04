// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Functional code for instance types.

use crate::latest::instance::{ErrorCode, InstanceProperties};

impl InstanceProperties {
    /// Return the name of the VMM resource backing this VM.
    pub fn vm_name(&self) -> String {
        self.id.to_string()
    }
}

impl std::fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        std::fmt::Debug::fmt(self, f)
    }
}

impl std::str::FromStr for ErrorCode {
    type Err = &'static str;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim() {
            s if s.eq_ignore_ascii_case("NoInstance") => Ok(Self::NoInstance),
            s if s.eq_ignore_ascii_case("AlreadyInitialized") => {
                Ok(ErrorCode::AlreadyInitialized)
            }
            s if s.eq_ignore_ascii_case("AlreadyRunning") => {
                Ok(ErrorCode::AlreadyRunning)
            }
            s if s.eq_ignore_ascii_case("CreateFailed") => {
                Ok(ErrorCode::CreateFailed)
            }
            _ => Err("unknown error code, expected one of: \
                'NoInstance', 'AlreadyInitialized', 'AlreadyRunning', \
                'CreateFailed'"),
        }
    }
}
