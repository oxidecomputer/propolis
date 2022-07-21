//! Helpers for configuring and starting new VMs.

use std::{
    net::{Ipv4Addr, SocketAddrV4},
    path::PathBuf,
};

use anyhow::Result;
use thiserror::Error;
use tracing::info;

use crate::{
    artifacts::ArtifactStore, guest_os::GuestOsKind,
    test_vm::ServerProcessParameters,
};

use super::{vm_config, TestVm};

#[derive(Debug, Error)]
pub enum FactoryConstructionError {
    #[error("Default bootrom {0} not in artifact store")]
    DefaultBootromMissing(String),

    #[error("Default guest image {0} not in artifact store")]
    DefaultGuestImageMissing(String),
}

#[derive(Debug)]
pub enum ServerLogMode {
    File(PathBuf),
    Stdio,
    Null,
}

#[derive(Debug)]
pub struct FactoryOptions {
    pub propolis_server_path: String,
    pub config_toml_path: PathBuf,
    pub server_log_mode: ServerLogMode,
    pub default_bootrom_artifact: String,
    pub default_guest_image_artifact: String,
    pub default_guest_cpus: u8,
    pub default_guest_memory_mib: u64,
}

pub struct VmFactory {
    opts: FactoryOptions,
    default_guest_image_path: String,
    default_guest_kind: GuestOsKind,
    default_bootrom_path: String,
}

impl VmFactory {
    pub fn new(opts: FactoryOptions, store: &ArtifactStore) -> Result<Self> {
        info!(?opts, "Building VM factory");
        let (guest_path, kind) = store
            .get_guest_image_by_name(&opts.default_guest_image_artifact)
            .ok_or(FactoryConstructionError::DefaultGuestImageMissing(
                opts.default_guest_image_artifact.clone(),
            ))?;

        let bootrom_path = store
            .get_bootrom_by_name(&opts.default_bootrom_artifact)
            .ok_or(FactoryConstructionError::DefaultBootromMissing(
                opts.default_bootrom_artifact.clone(),
            ))?;

        Ok(Self {
            opts,
            default_guest_image_path: guest_path.to_string_lossy().to_string(),
            default_guest_kind: kind,
            default_bootrom_path: bootrom_path.to_string_lossy().to_string(),
        })
    }

    pub fn default_vm_config(&self) -> vm_config::VmConfigBuilder {
        self.deviceless_vm_config()
            .add_virtio_block_disk(&self.default_guest_image_path, 4)
    }

    pub fn deviceless_vm_config(&self) -> vm_config::VmConfigBuilder {
        let bootrom_path =
            PathBuf::try_from(&self.default_bootrom_path).unwrap();
        vm_config::VmConfigBuilder::new()
            .set_bootrom_path(bootrom_path)
            .set_cpus(self.opts.default_guest_cpus)
            .set_memory_mib(self.opts.default_guest_memory_mib)
    }

    pub fn new_vm(
        &self,
        vm_name: &str,
        builder: vm_config::VmConfigBuilder,
    ) -> Result<TestVm> {
        let vm_config = builder.finish();
        info!(?vm_name, ?vm_config);
        vm_config.write_config_toml(&self.opts.config_toml_path)?;

        let (server_stdout, server_stderr) = match &self.opts.server_log_mode {
            ServerLogMode::File(dir) => {
                let mut stdout_path = dir.clone();
                stdout_path.push(format!("{}.stdout.log", vm_name));
                let mut stderr_path = dir.clone();
                stderr_path.push(format!("{}.stderr.log", vm_name));
                info!(?stdout_path, ?stderr_path, "Opening server log files");
                (
                    std::fs::File::create(stdout_path)?.into(),
                    std::fs::File::create(stderr_path)?.into(),
                )
            }
            ServerLogMode::Stdio => {
                (std::process::Stdio::inherit(), std::process::Stdio::inherit())
            }
            ServerLogMode::Null => {
                (std::process::Stdio::null(), std::process::Stdio::null())
            }
        };

        let server_params = ServerProcessParameters {
            server_path: &self.opts.propolis_server_path,
            config_toml_path: &self
                .opts
                .config_toml_path
                .as_os_str()
                .to_string_lossy(),
            server_addr: SocketAddrV4::new(Ipv4Addr::new(127, 0, 0, 1), 9000),
            server_stdout,
            server_stderr,
        };

        TestVm::new(vm_name, server_params, &vm_config, self.default_guest_kind)
    }
}
