// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The Pheidippides framework: interfaces for creating and interacting with
//! VMs.
//!
//! This module defines a `Framework` object that contains a set of default VM
//! parameters (shape, bootrom, boot disk image) and the context needed to
//! launch new guest VMs (paths, logging options, an artifact store, etc.). The
//! PHD runner process instantiates a `Framework` and then passes a reference to
//! it to each PHD test case. Test cases then use the `Framework`'s public
//! interface to create test VMs with various configurations.
//!
//! Tests are expected to access `Framework` functions to create VMs and public
//! `TestVm` functions to work directly with those VMs. Most other functionality
//! in this crate is private to the crate.
//!
//! To launch a VM, the framework needs to know how to configure the VM itself
//! and how to run the Propolis server process that will host it. The framework
//! supplies builders, `vm_builder` and `environment_builder`, that allow tests
//! to configure both of these options.
//!
//! Often, tests will want to spawn a "successor" to an existing VM that
//! maintains the VM's configuration and any related objects but that runs in a
//! separate Propolis server process that may have been spawned in a different
//! environment. The `spawn_successor_vm` function provides a shorthand way to
//! do this.

use std::{ops::Range, rc::Rc};

use anyhow::Context;
use artifacts::DEFAULT_PROPOLIS_ARTIFACT;
use camino::Utf8PathBuf;

use disk::DiskFactory;
use port_allocator::PortAllocator;
use server_log_mode::ServerLogMode;
pub use test_vm::TestVm;
use test_vm::{
    environment::EnvironmentSpec, spec::VmSpec, VmConfig, VmLocation,
};

pub mod artifacts;
pub mod disk;
pub mod guest_os;
pub mod host_api;
pub mod lifecycle;
mod port_allocator;
mod serial;
pub mod server_log_mode;
pub mod test_vm;

/// An instance of the PHD test framework.
pub struct Framework {
    pub(crate) tmp_directory: Utf8PathBuf,
    pub(crate) server_log_mode: ServerLogMode,

    pub(crate) default_guest_cpus: u8,
    pub(crate) default_guest_memory_mib: u64,
    pub(crate) default_guest_os_artifact: String,
    pub(crate) default_bootrom_artifact: String,

    pub(crate) artifact_store: Rc<artifacts::ArtifactStore>,
    pub(crate) disk_factory: DiskFactory,
    pub(crate) port_allocator: Rc<PortAllocator>,
}

pub struct FrameworkParameters {
    pub propolis_server_path: Utf8PathBuf,
    pub crucible_downstairs_cmd: Option<Utf8PathBuf>,

    pub tmp_directory: Utf8PathBuf,
    pub artifact_toml: Utf8PathBuf,
    pub server_log_mode: ServerLogMode,

    pub default_guest_cpus: u8,
    pub default_guest_memory_mib: u64,
    pub default_guest_os_artifact: String,
    pub default_bootrom_artifact: String,

    pub port_range: Range<u16>,
}

// The framework implementation includes some "runner-only" functions
// (constructing and resetting a framework) that are marked `pub`. This could be
// improved by splitting the "test case" functions into a trait and giving test
// cases trait objects.
impl Framework {
    /// Builds a brand new framework. Called from the test runner, which creates
    /// one framework and then distributes it to tests.
    pub fn new(params: FrameworkParameters) -> anyhow::Result<Self> {
        let mut artifact_store = artifacts::ArtifactStore::from_toml_path(
            params.tmp_directory.clone(),
            &params.artifact_toml,
        )
        .context("creating PHD framework")?;

        artifact_store
            .add_propolis_from_local_cmd(&params.propolis_server_path)
            .with_context(|| {
                format!(
                    "adding Propolis server '{}' from options",
                    &params.propolis_server_path
                )
            })?;

        let artifact_store = Rc::new(artifact_store);
        let port_allocator = Rc::new(PortAllocator::new(params.port_range));
        let disk_factory = DiskFactory::new(
            &params.tmp_directory,
            artifact_store.clone(),
            params.crucible_downstairs_cmd.clone().as_ref(),
            port_allocator.clone(),
            params.server_log_mode,
        );

        Ok(Self {
            tmp_directory: params.tmp_directory,
            server_log_mode: params.server_log_mode,
            default_guest_cpus: params.default_guest_cpus,
            default_guest_memory_mib: params.default_guest_memory_mib,
            default_guest_os_artifact: params.default_guest_os_artifact,
            default_bootrom_artifact: params.default_bootrom_artifact,
            artifact_store,
            disk_factory,
            port_allocator,
        })
    }

    /// Resets the state of any stateful objects in the framework to prepare it
    /// to run a new test case.
    pub fn reset(&self) {
        self.port_allocator.reset();
    }

    /// Creates a new VM configuration builder using the default configuration
    /// from this framework instance.
    pub fn vm_config_builder(&self, vm_name: &str) -> VmConfig {
        VmConfig::new(
            vm_name,
            self.default_guest_cpus,
            self.default_guest_memory_mib,
            &self.default_bootrom_artifact,
            &self.default_guest_os_artifact,
        )
    }

    /// Yields an environment builder with default settings (run the VM on the
    /// test runner's machine using the default Propolis from the command line).
    pub fn environment_builder(&self) -> EnvironmentSpec {
        EnvironmentSpec::new(VmLocation::Local, DEFAULT_PROPOLIS_ARTIFACT)
    }

    /// Spawns a test VM using the default configuration returned from
    /// `vm_builder` and the default environment returned from
    /// `environment_builder`.
    pub fn spawn_default_vm(&self, vm_name: &str) -> anyhow::Result<TestVm> {
        self.spawn_vm(&self.vm_config_builder(vm_name), None)
    }

    /// Spawns a new test VM using the supplied `config`. If `environment` is
    /// `Some`, the VM is spawned using the supplied environment; otherwise it
    /// is spawned using the default `environment_builder`.
    pub fn spawn_vm(
        &self,
        config: &VmConfig,
        environment: Option<&EnvironmentSpec>,
    ) -> anyhow::Result<TestVm> {
        TestVm::new(
            self,
            config.vm_spec(self).context("building VM config for test VM")?,
            environment.unwrap_or(&self.environment_builder()),
        )
        .context("constructing test VM")
    }

    /// Spawns a "successor" to the supplied `vm`. The successor has the same
    /// configuration and takes additional references to all of its
    /// predecessor's backing objects (e.g. disk handles). If `environment` is
    /// `None`, the successor is launched using the predecessor's environment
    /// spec.
    pub fn spawn_successor_vm(
        &self,
        vm_name: &str,
        vm: &TestVm,
        environment: Option<&EnvironmentSpec>,
    ) -> anyhow::Result<TestVm> {
        let mut vm_spec =
            VmSpec { vm_name: vm_name.to_owned(), ..vm.vm_spec() };

        // Reconcile any differences between the generation numbers in the VM
        // objects' instance spec and the associated Crucible disk handles.
        // This may be needed because a test can call `set_generation` on a disk
        // handle to change its active generation number mid-test, and this
        // won't automatically be reflected in the VM's instance spec.
        vm_spec.refresh_crucible_backends();
        TestVm::new(
            self,
            vm_spec,
            environment.unwrap_or(&vm.environment_spec()),
        )
    }

    /// Yields this framework instance's default guest OS artifact name. This
    /// can be used to configure boot disks with different parameters than the
    /// builder defaults.
    pub fn default_guest_os_artifact(&self) -> &str {
        &self.default_guest_os_artifact
    }

    /// Indicates whether the disk factory in this framework supports the
    /// creation of Crucible disks. This can be used to skip tests that require
    /// Crucible support.
    pub fn crucible_enabled(&self) -> bool {
        self.disk_factory.crucible_enabled()
    }
}
