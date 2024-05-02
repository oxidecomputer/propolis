// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::net::{Ipv4Addr, SocketAddrV4};

use anyhow::Context;

use crate::{test_vm::server::ServerProcessParameters, Framework};

/// Specifies where the framework should start a new test VM.
#[derive(Clone, Copy, Debug)]
pub enum VmLocation {
    /// Start the VM on the system where the test runner is executing.
    Local,
    // TODO: Support remote VMs.
}

#[derive(Clone, Debug)]
pub struct EnvironmentSpec {
    pub(crate) location: VmLocation,
    pub(crate) propolis_artifact: String,
}

impl EnvironmentSpec {
    pub(crate) fn new(location: VmLocation, propolis_artifact: &str) -> Self {
        Self { location, propolis_artifact: propolis_artifact.to_owned() }
    }

    pub fn location(&mut self, location: VmLocation) -> &mut Self {
        self.location = location;
        self
    }

    pub fn propolis(&mut self, artifact_name: &str) -> &mut Self {
        artifact_name.clone_into(&mut self.propolis_artifact);
        self
    }

    pub(crate) async fn build<'a>(
        &self,
        framework: &'a Framework,
    ) -> anyhow::Result<Environment<'a>> {
        Environment::from_builder(self, framework).await
    }
}

/// Specifies all of the details the framework needs to stand up a VM in a
/// specific environment.
///
/// When tests want to spawn a new VM, they pass a `VmLocation` to the
/// framework, and the framework augments that with
#[derive(Clone, Debug)]
pub(crate) enum Environment<'a> {
    Local(ServerProcessParameters<'a>),
}

impl<'a> Environment<'a> {
    async fn from_builder(
        builder: &EnvironmentSpec,
        framework: &'a Framework,
    ) -> anyhow::Result<Self> {
        match builder.location {
            VmLocation::Local => {
                let propolis_server = framework
                    .artifact_store
                    .get_propolis_server(&builder.propolis_artifact)
                    .await
                    .context("setting up VM execution environment")?;
                let server_port = framework
                    .port_allocator
                    .next()
                    .context("getting Propolis server port")?;
                let vnc_port = framework
                    .port_allocator
                    .next()
                    .context("getting VNC server port")?;
                let params = ServerProcessParameters {
                    server_path: propolis_server,
                    data_dir: framework.tmp_directory.as_path(),
                    server_addr: SocketAddrV4::new(
                        Ipv4Addr::new(127, 0, 0, 1),
                        server_port,
                    ),
                    vnc_addr: SocketAddrV4::new(
                        Ipv4Addr::new(127, 0, 0, 1),
                        vnc_port,
                    ),
                    log_mode: framework.server_log_mode,
                };
                Ok(Self::Local(params))
            }
        }
    }
}
