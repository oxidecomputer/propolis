//! Types and helpers specifying where a server process's stdout/stderr should
//! be recorded.

use std::{path::Path, process::Stdio, str::FromStr};

use tracing::info;

/// Specifies where a process's output should be written.
#[derive(Debug, Clone, Copy)]
pub enum ServerLogMode {
    /// Write to files in the server's factory's temporary directory.
    TmpFile,

    /// Write stdout/stderr to the console.
    Stdio,

    /// Redirect stdout/stderr to /dev/null.
    Null,
}

impl FromStr for ServerLogMode {
    type Err = std::io::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "file" | "tmpfile" => Ok(ServerLogMode::TmpFile),
            "stdio" => Ok(ServerLogMode::Stdio),
            "null" => Ok(ServerLogMode::Null),
            _ => Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                s.to_string(),
            )),
        }
    }
}

impl ServerLogMode {
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
            ServerLogMode::TmpFile => {
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
            ServerLogMode::Stdio => Ok((Stdio::inherit(), Stdio::inherit())),
            ServerLogMode::Null => Ok((Stdio::null(), Stdio::null())),
        }
    }
}
