// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Routines and data structures for working with Propolis server processes.

use std::{fmt::Debug, net::SocketAddrV4};

use anyhow::Result;
use camino::{Utf8Path, Utf8PathBuf};
use tracing::info;

use crate::server_log_mode::ServerLogMode;

/// Parameters used to launch and configure the Propolis server process. These
/// are distinct from the parameters used to configure the VM that that process
/// will host.
#[derive(Clone, Debug)]
pub struct ServerProcessParameters<'a> {
    /// The path to the server binary to launch.
    pub server_path: Utf8PathBuf,

    /// The directory in which to find or place files that are read or written
    /// by this server process.
    pub data_dir: &'a Utf8Path,

    /// The address at which the server should serve.
    pub server_addr: SocketAddrV4,

    /// The address at which the server should offer its VNC server.
    pub vnc_addr: SocketAddrV4,

    pub log_mode: ServerLogMode,
}

pub struct PropolisServer {
    server: std::process::Child,
    address: SocketAddrV4,
}

impl PropolisServer {
    pub(crate) fn new(
        vm_name: &str,
        process_params: ServerProcessParameters,
        config_toml_path: &Utf8Path,
    ) -> Result<Self> {
        let ServerProcessParameters {
            server_path,
            data_dir,
            server_addr,
            vnc_addr,
            log_mode,
        } = process_params;

        info!(
            ?server_path,
            ?config_toml_path,
            ?server_addr,
            "Launching Propolis server"
        );

        let (server_stdout, server_stderr) =
            log_mode.get_handles(&data_dir, vm_name)?;

        let server = PropolisServer {
            server: std::process::Command::new("pfexec")
                .args([
                    server_path.as_str(),
                    "run",
                    config_toml_path.as_str(),
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
