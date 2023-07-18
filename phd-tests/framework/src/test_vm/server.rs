// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Routines and data structures for working with Propolis server processes.

use std::{fmt::Debug, net::SocketAddrV4, path::PathBuf, process::Stdio};

use anyhow::Result;
use tracing::info;

/// Parameters used to launch and configure the Propolis server process. These
/// are distinct from the parameters used to configure the VM that that process
/// will host.
#[derive(Debug)]
pub struct ServerProcessParameters<'a, T: Into<Stdio>> {
    /// The path to the server binary to launch.
    pub server_path: &'a str,

    /// The path to the configuration TOML that should be placed on the server's
    /// command line.
    pub config_toml_path: PathBuf,

    /// The address at which the server should serve.
    pub server_addr: SocketAddrV4,

    /// The address at which the server should offer its VNC server.
    pub vnc_addr: SocketAddrV4,

    /// The [`Stdio`] descriptor to which the server's stdout should be
    /// directed.
    pub server_stdout: T,

    /// The [`Stdio`] descriptor to which the server's stderr should be
    /// directed.
    pub server_stderr: T,
}

pub struct PropolisServer {
    server: std::process::Child,
    address: SocketAddrV4,
}

impl PropolisServer {
    pub(crate) fn new<T: Into<Stdio> + Debug>(
        process_params: ServerProcessParameters<T>,
    ) -> Result<Self> {
        let ServerProcessParameters {
            server_path,
            config_toml_path,
            server_addr,
            vnc_addr,
            server_stdout,
            server_stderr,
        } = process_params;

        info!(
            ?server_path,
            ?config_toml_path,
            ?server_addr,
            "Launching Propolis server"
        );

        let server = PropolisServer {
            server: std::process::Command::new("pfexec")
                .args([
                    server_path,
                    "run",
                    config_toml_path.as_os_str().to_string_lossy().as_ref(),
                    server_addr.to_string().as_str(),
                    vnc_addr.to_string().as_str(),
                ])
                .stdout(server_stdout)
                .stderr(server_stderr)
                .spawn()?,
            address: server_addr,
        };

        info!("Launched server with pid {}", server.server.id());
        Ok(server)
    }

    pub(crate) fn server_addr(&self) -> SocketAddrV4 {
        self.address
    }
}

impl Drop for PropolisServer {
    fn drop(&mut self) {
        let pid = self.server.id().to_string();
        info!(
            pid,
            %self.address,
            "Killing Propolis server that was dropped"
        );

        std::process::Command::new("pfexec")
            .args(["kill", &pid])
            .spawn()
            .expect("should be able to kill a phd-spawned propolis");

        self.server
            .wait()
            .expect("should be able to wait on a phd-spawned propolis");

        info!(pid,
              %self.address,
              "Successfully waited for demise of Propolis server that was dropped");
    }
}
