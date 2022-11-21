//! Helpers for configuring and starting new VMs.

use std::{
    net::{Ipv4Addr, SocketAddrV4},
    path::PathBuf,
};

use anyhow::Result;
use thiserror::Error;
use tracing::info;

use crate::{
    artifacts::ArtifactStore, port_allocator::PortAllocator,
    server_log_mode::ServerLogMode, test_vm::server::ServerProcessParameters,
};

use super::{
    vm_config::{self, VmConfig},
    TestVm,
};

/// Errors that can arise while creating a VM factory.
#[derive(Debug, Error)]
pub enum FactoryConstructionError {
    /// Raised if the default bootrom key in the [`FactoryOptions`] does not
    /// yield a valid bootrom from the artifact store.
    #[error("Default bootrom {0} not in artifact store")]
    DefaultBootromMissing(String),

    /// Raised if the default guest image key in the [`FactoryOptions`] does not
    /// yield a valid image from the artifact store.
    #[error("Default guest image {0} not in artifact store")]
    DefaultGuestImageMissing(String),

    /// Raised on a failure to convert from a named server logging mode to a
    /// [`ServerLogMode`].
    #[error("Invalid server log mode name '{0}'")]
    InvalidServerLogModeName(String),
}

/// Parameters used to construct a new VM factory.
#[derive(Debug)]
pub struct FactoryOptions {
    /// The path to the Propolis server binary to use for VMs created by this
    /// factory.
    pub propolis_server_path: String,

    /// The directory to use as a temporary directory for config TOML files,
    /// server logs, and the like.
    pub tmp_directory: PathBuf,

    /// The logging discipline to use for this factory's Propolis servers.
    pub server_log_mode: ServerLogMode,

    /// An artifact store key specifying the default bootrom artifact to use for
    /// this factory's VMs.
    pub default_bootrom_artifact: String,

    /// The default number of CPUs to set in [`vm_config::VmConfig`] structs
    /// generated by this factory.
    pub default_guest_cpus: u8,

    /// The default amount of memory to set in [`vm_config::VmConfig`] structs
    /// generated by this factory.
    pub default_guest_memory_mib: u64,
}

/// A VM factory that provides routines to generate new test VMs.
pub struct VmFactory<'a> {
    opts: FactoryOptions,
    default_bootrom_path: String,
    port_allocator: &'a PortAllocator,
}

impl<'a> VmFactory<'a> {
    /// Creates a new VM factory with default bootrom/guest image artifacts
    /// drawn from the supplied artifact store.
    pub fn new(
        opts: FactoryOptions,
        store: &ArtifactStore,
        port_allocator: &'a PortAllocator,
    ) -> Result<Self> {
        info!(?opts, "Building VM factory");
        let bootrom_path = store
            .get_bootrom_by_name(&opts.default_bootrom_artifact)
            .ok_or_else(|| {
                FactoryConstructionError::DefaultBootromMissing(
                    opts.default_bootrom_artifact.clone(),
                )
            })?;

        Ok(Self {
            opts,
            default_bootrom_path: bootrom_path.to_string_lossy().to_string(),
            port_allocator,
        })
    }
}

impl VmFactory<'_> {
    /// Resets this factory to the state it had when it was created, preparing
    /// it for use in a new test case.
    pub fn reset(&self) {
        self.port_allocator.reset();
    }

    /// Creates a VM configuration that specifies this factory's defaults for
    /// CPUs, memory, and bootrom.
    pub fn deviceless_vm_config(&self) -> vm_config::ConfigRequest {
        let bootrom_path =
            PathBuf::try_from(&self.default_bootrom_path).unwrap();
        vm_config::ConfigRequest::new()
            .set_bootrom_path(bootrom_path)
            .set_cpus(self.opts.default_guest_cpus)
            .set_memory_mib(self.opts.default_guest_memory_mib)
    }

    /// Launches a new Propolis server process with a VM configuration created
    /// by the supplied configuration builder. Returns the [`TestVm`] associated
    /// with this server.
    pub fn new_vm(
        &self,
        vm_name: &str,
        config: vm_config::ConfigRequest,
    ) -> Result<TestVm> {
        info!(?vm_name, ?config, "Configuring VM from request");
        let realized = config.finish(
            &self.opts.tmp_directory,
            &format!("{}.config.toml", vm_name),
        )?;

        self.create_vm_from_config(vm_name, realized)
    }

    /// Launches a new Propolis server process with a VM configuration cloned
    /// from an existing VM. Returns the [`TestVm`] associated with this server.
    ///
    /// This is useful for live migration tests where the source and target are
    /// expected to have identical configuration data.
    pub fn new_vm_from_cloned_config(
        &self,
        vm_name: &str,
        vm_to_clone: &TestVm,
    ) -> Result<TestVm> {
        let config = vm_to_clone.clone_config();
        let server_toml_path = config.server_toml_path();
        let mut new_toml_path =
            server_toml_path.parent().unwrap().to_path_buf();
        new_toml_path.push(format!("{}.config.toml", vm_name));
        info!(
            ?server_toml_path,
            ?new_toml_path,
            "Copying existing server config TOML"
        );
        std::fs::copy(server_toml_path, &new_toml_path)?;

        self.create_vm_from_config(vm_name, config)
    }

    fn create_vm_from_config(
        &self,
        vm_name: &str,
        vm_config: VmConfig,
    ) -> Result<TestVm> {
        let (server_stdout, server_stderr) = self
            .opts
            .server_log_mode
            .get_handles(&self.opts.tmp_directory, vm_name)?;

        let server_port = self.port_allocator.next()?;
        let vnc_port = self.port_allocator.next()?;
        let server_params = ServerProcessParameters {
            server_path: &self.opts.propolis_server_path,
            config_toml_path: vm_config.server_toml_path().clone(),
            server_addr: SocketAddrV4::new(
                Ipv4Addr::new(127, 0, 0, 1),
                server_port,
            ),
            vnc_addr: SocketAddrV4::new(Ipv4Addr::new(127, 0, 0, 1), vnc_port),
            server_stdout,
            server_stderr,
        };

        TestVm::new(vm_name, server_params, vm_config)
    }
}
