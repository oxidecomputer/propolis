// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Types and helpers specifying how logs should be formatted and where they
//! should be directed.

use std::{path::Path, process::Stdio, str::FromStr};

use tracing::info;

/// Specifies how a test's logging should be managed.
#[derive(Debug, Clone, Copy)]
pub struct LogConfig {
    pub output_mode: OutputMode,
    pub log_format: LogFormat,
}

/// Specifies where a output for a test's processes should be written.
#[derive(Debug, Clone, Copy)]
pub enum OutputMode {
    /// Write to files in the server's factory's temporary directory.
    TmpFile,

    /// Write stdout/stderr to the console.
    Stdio,

    /// Redirect stdout/stderr to /dev/null.
    Null,
}

impl FromStr for OutputMode {
    type Err = std::io::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "file" | "tmpfile" => Ok(OutputMode::TmpFile),
            "stdio" => Ok(OutputMode::Stdio),
            "null" => Ok(OutputMode::Null),
            _ => Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                s.to_string(),
            )),
        }
    }
}

impl OutputMode {
    /// Returns the stdout/stderr handles to pass to processes using the
    /// specified logging mode.
    ///
    /// # Parameters
    ///
    /// - directory: The directory in which to store any files written under
    ///   the selected discipline.
    /// - file_prefix: The prefix to add to the names of any files written
    ///   under the selected discipline.
    pub(crate) fn get_handles(
        &self,
        directory: &impl AsRef<Path>,
        file_prefix: &str,
    ) -> anyhow::Result<(Stdio, Stdio)> {
        match self {
            OutputMode::TmpFile => {
                let mut stdout_path = directory.as_ref().to_path_buf();
                stdout_path.push(format!("{}.stdout.log", file_prefix));

                let mut stderr_path = directory.as_ref().to_path_buf();
                stderr_path.push(format!("{}.stderr.log", file_prefix));

                info!(?stdout_path, ?stderr_path, "Opening server log files");
                Ok((
                    std::fs::File::create(stdout_path)?.into(),
                    std::fs::File::create(stderr_path)?.into(),
                ))
            }
            OutputMode::Stdio => Ok((Stdio::inherit(), Stdio::inherit())),
            OutputMode::Null => Ok((Stdio::null(), Stdio::null())),
        }
    }
}

/// Specifies how output for a test's processes should be structured.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogFormat {
    /// Format logs as plain hopefully human-readable output.
    Plain,

    /// Format logs as Bunyan output, more suitable for machine processing (such
    /// as in CI).
    Bunyan,
}
