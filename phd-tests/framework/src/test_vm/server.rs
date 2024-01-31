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

        // In the normal course of events, dropping a `TestVm` will spawn a task
        // that takes ownership of its `PropolisServer` and then sends Propolis
        // client calls to gracefully stop the VM running in that server (if
        // there is one). This ensures (modulo any bugs in Propolis itself) that
        // any Propolis servers hosting a running VM will have a chance to
        // destroy their kernel VMMs before the servers themselves go away.
        //
        // The PHD runner tries to give VMs a chance to tear down gracefully on
        // Ctrl-C by installing a custom SIGINT handler and using it to tell
        // running test tasks to shut down instead of completing their tests.
        // This allows the tests' VMs to be dropped gracefully. The trouble is
        // that with no additional intervention, pressing Ctrl-C to interrupt a
        // PHD runner process will typically cause SIGINTs to be delivered to
        // every process in the foreground process group, including both the
        // runner and all its Propolis server children. This breaks the `TestVm`
        // cleanup logic: by the time the VM is cleaned up, its server process
        // is already gone (due to the signal), so its VMM can't be cleaned up.
        //
        // To get around this, spawn Propolis server processes with a pre-`exec`
        // hook that directs them to ignore SIGINT. They will still be killed
        // during `TestVm` cleanup, assuming that executes normally; if the
        // runner is rudely terminated after that, the server process is still
        // preserved for investigation (and can still be cleaned up by other
        // means, e.g. SIGKILL).
        //
        // Note that it's undesirable for PHD to try to clean up leaked kernel
        // VMMs itself: leaking a kernel VMM after gracefully stopping a
        // Propolis server VM indicates a Propolis server bug.
        let server = PropolisServer {
            server: unsafe {
                std::process::Command::new("pfexec")
                    .args([
                        server_path.as_str(),
                        "run",
                        config_toml_path.as_str(),
                        server_addr.to_string().as_str(),
                        vnc_addr.to_string().as_str(),
                    ])
                    .stdout(server_stdout)
                    .stderr(server_stderr)
                    .pre_exec(move || {
                        libc::signal(libc::SIGINT, libc::SIG_IGN);
                        Ok(())
                    })
                    .spawn()?
            },
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
        debug!(
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

        debug!(pid,
              %self.address,
              "Successfully waited for demise of Propolis server that was dropped");
    }
}
