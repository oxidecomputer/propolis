// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Routines and data structures for working with Propolis server processes.

use std::{fmt::Debug, net::SocketAddrV4, os::unix::process::CommandExt};

use anyhow::Result;
use camino::{Utf8Path, Utf8PathBuf};
use tracing::{debug, info};

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
    server: Option<std::process::Child>,
    address: SocketAddrV4,
}

impl PropolisServer {
    pub(crate) fn new(
        vm_name: &str,
        process_params: ServerProcessParameters,
        bootrom_path: &Utf8Path,
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
            ?bootrom_path,
            ?server_addr,
            "Launching Propolis server"
        );

        let (server_stdout, server_stderr) =
            log_mode.get_handles(&data_dir, vm_name)?;

        let mut server_cmd = std::process::Command::new("pfexec");
        server_cmd
            .args([
                server_path.as_str(),
                "run",
                bootrom_path.as_str(),
                server_addr.to_string().as_str(),
                vnc_addr.to_string().as_str(),
            ])
            .stdout(server_stdout)
            .stderr(server_stderr);

        // Gracefully shutting down a Propolis server requires PHD to send an
        // instance stop request to the server before it is actually terminated.
        // This ensures that the server has a chance to clean up kernel VMM
        // resources. It's desirable for the server to do this and not PHD
        // because a failure to clean up VMMs on stop is a Propolis bug.
        //
        // The PHD runner sets up a SIGINT handler that tries to give the
        // framework an opportunity to issue these requests before the runner
        // exits. However, pressing Ctrl-C in a shell will typically broadcast
        // SIGINT to all of the processes in the foreground process's group, not
        // just to the foreground process itself. This means that a Ctrl-C press
        // will usually kill all of PHD's Propolis servers before the cleanup
        // logic can run.
        //
        // To avoid this problem, add a pre-`exec` hook that directs Propolis
        // servers to ignore SIGINT. On Ctrl-C, the runner will drop all active
        // `TestVm`s, and this drop path (if allowed to complete) will kill all
        // these processes.
        unsafe {
            server_cmd.pre_exec(move || {
                libc::signal(libc::SIGINT, libc::SIG_IGN);
                Ok(())
            });
        }

        let server = PropolisServer {
            server: Some(server_cmd.spawn()?),
            address: server_addr,
        };

        info!(
            "Launched server with pid {}",
            server.server.as_ref().unwrap().id()
        );
        Ok(server)
    }

    pub(crate) fn server_addr(&self) -> SocketAddrV4 {
        self.address
    }

    /// Kills this server process if it hasn't been killed already.
    pub(super) fn kill(&mut self) {
        let Some(mut server) = self.server.take() else {
            return;
        };

        let pid = server.id();
        debug!(
            pid,
            %self.address,
            "Killing Propolis server process"
        );

        std::process::Command::new("pfexec")
            .args(["kill", &pid.to_string()])
            .spawn()
            .expect("should be able to kill a phd-spawned propolis");

        server
            .wait()
            .expect("should be able to wait on a phd-spawned propolis");
    }
}

impl Drop for PropolisServer {
    fn drop(&mut self) {
        self.kill();
    }
}
