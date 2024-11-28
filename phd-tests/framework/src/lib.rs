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

use std::{fmt, ops::Range, sync::Arc};

use anyhow::Context;
use artifacts::DEFAULT_PROPOLIS_ARTIFACT;
use camino::Utf8PathBuf;

use disk::DiskFactory;
use futures::{stream::FuturesUnordered, StreamExt};
use guest_os::GuestOsKind;
use port_allocator::PortAllocator;
use server_log_mode::ServerLogMode;
pub use test_vm::TestVm;
use test_vm::{
    environment::EnvironmentSpec, spec::VmSpec, VmConfig, VmLocation,
};
use tokio::{
    sync::mpsc::{UnboundedReceiver, UnboundedSender},
    task::JoinHandle,
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
pub(crate) mod zfs;

/// An instance of the PHD test framework.
pub struct Framework {
    pub(crate) tmp_directory: Utf8PathBuf,
    pub(crate) server_log_mode: ServerLogMode,

    pub(crate) default_guest_cpus: u8,
    pub(crate) default_guest_memory_mib: u64,
    pub(crate) default_guest_os_artifact: String,
    pub(crate) default_bootrom_artifact: String,

    // The disk factory used to be a freestanding struct that took references to
    // an artifact store and port allocator that were owned by someone else.
    // Putting all these components into a single struct makes the struct
    // self-referencing. Since the runner is single-threaded, avoid arguing with
    // anyone about lifetimes by wrapping the relevant shared components in an
    // `Rc`.
    pub(crate) artifact_store: Arc<artifacts::ArtifactStore>,
    pub(crate) disk_factory: DiskFactory,
    pub(crate) port_allocator: Arc<PortAllocator>,

    pub(crate) crucible_enabled: bool,
    pub(crate) migration_base_enabled: bool,

    /// Buffers cleanup tasks that need to be run after a test case completes.
    /// [`Self::cleanup_task_channel`] returns a clone of this sender that
    /// framework users can use to register these tasks (without having to hold
    /// a reference to the `Framework`).
    cleanup_task_tx: UnboundedSender<JoinHandle<()>>,

    /// The receiver side of [`cleanup_task_tx`].
    cleanup_task_rx: tokio::sync::Mutex<UnboundedReceiver<JoinHandle<()>>>,
}

pub struct FrameworkParameters<'a> {
    pub propolis_server_path: Utf8PathBuf,
    pub crucible_downstairs: Option<CrucibleDownstairsSource>,
    pub base_propolis: Option<BasePropolisSource<'a>>,

    pub tmp_directory: Utf8PathBuf,
    pub artifact_directory: Utf8PathBuf,
    pub artifact_toml: Utf8PathBuf,
    pub server_log_mode: ServerLogMode,

    pub default_guest_cpus: u8,
    pub default_guest_memory_mib: u64,
    pub default_guest_os_artifact: String,
    pub default_bootrom_artifact: String,

    pub port_range: Range<u16>,
    pub max_buildomat_wait: std::time::Duration,
}

#[derive(Debug)]
pub enum CrucibleDownstairsSource {
    BuildomatGitRev(artifacts::buildomat::Commit),
    Local(Utf8PathBuf),
}

#[derive(Debug, Copy, Clone)]
pub enum BasePropolisSource<'a> {
    BuildomatGitRev(&'a artifacts::buildomat::Commit),
    BuildomatBranch(&'a str),
    Local(&'a Utf8PathBuf),
}

// The framework implementation includes some "runner-only" functions
// (constructing and resetting a framework) that are marked `pub`. This could be
// improved by splitting the "test case" functions into a trait and giving test
// cases trait objects.
impl Framework {
    /// Builds a brand new framework. Called from the test runner, which creates
    /// one framework and then distributes it to tests.
    pub async fn new(params: FrameworkParameters<'_>) -> anyhow::Result<Self> {
        let mut artifact_store = artifacts::ArtifactStore::from_toml_path(
            params.artifact_directory.clone(),
            &params.artifact_toml,
            params.max_buildomat_wait,
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

        let crucible_enabled = match params.crucible_downstairs {
            Some(source) => {
                artifact_store
                    .add_crucible_downstairs(&source)
                    .await
                    .with_context(|| {
                        format!(
                            "adding Crucible downstairs {source} from options",
                        )
                    })?;
                true
            }
            None => {
                tracing::warn!(
                    "Crucible disabled. Crucible tests will be skipped"
                );
                false
            }
        };

        let migration_base_enabled = match params.base_propolis {
            Some(source) => {
                artifact_store
                    .add_current_propolis(source)
                    .await
                    .with_context(|| format!("adding 'migration base' Propolis server {source} from options"))?;
                true
            }
            None => {
                tracing::warn!("No 'migration base' Propolis server provided. Migration-from-base tests will be skipped.");
                false
            }
        };

        let artifact_store = Arc::new(artifact_store);
        let port_allocator = Arc::new(PortAllocator::new(params.port_range));
        let disk_factory = DiskFactory::new(
            &params.tmp_directory,
            artifact_store.clone(),
            port_allocator.clone(),
            params.server_log_mode,
        );

        let (cleanup_task_tx, cleanup_task_rx) =
            tokio::sync::mpsc::unbounded_channel();
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
            crucible_enabled,
            migration_base_enabled,
            cleanup_task_tx,
            cleanup_task_rx: tokio::sync::Mutex::new(cleanup_task_rx),
        })
    }

    /// Resets the state of any stateful objects in the framework to prepare it
    /// to run a new test case.
    pub async fn reset(&self) {
        self.port_allocator.reset();
        self.wait_for_cleanup_tasks().await;
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
    pub async fn spawn_default_vm(
        &self,
        vm_name: &str,
    ) -> anyhow::Result<TestVm> {
        self.spawn_vm(&self.vm_config_builder(vm_name), None).await
    }

    /// Spawns a new test VM using the supplied `config`. If `environment` is
    /// `Some`, the VM is spawned using the supplied environment; otherwise it
    /// is spawned using the default `environment_builder`.
    pub async fn spawn_vm<'dr>(
        &self,
        config: &VmConfig<'dr>,
        environment: Option<&EnvironmentSpec>,
    ) -> anyhow::Result<TestVm> {
        TestVm::new(
            self,
            config
                .vm_spec(self)
                .await
                .context("building VM config for test VM")?,
            environment.unwrap_or(&self.environment_builder()),
        )
        .await
        .context("constructing test VM")
    }

    /// Spawns a "successor" to the supplied `vm`. The successor has the same
    /// configuration and takes additional references to all of its
    /// predecessor's backing objects (e.g. disk handles). If `environment` is
    /// `None`, the successor is launched using the predecessor's environment
    /// spec.
    pub async fn spawn_successor_vm(
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

        // Create new metadata for an instance based on this predecessor. It
        // should have the same project and silo IDs, but the sled identifiers
        // will be different.
        vm_spec.refresh_sled_identifiers();

        TestVm::new(
            self,
            vm_spec,
            environment.unwrap_or(&vm.environment_spec()),
        )
        .await
    }

    /// Yields this framework instance's default guest OS artifact name. This
    /// can be used to configure boot disks with different parameters than the
    /// builder defaults.
    pub fn default_guest_os_artifact(&self) -> &str {
        &self.default_guest_os_artifact
    }

    /// Yields the guest OS adapter corresponding to the default guest OS
    /// artifact.
    pub async fn default_guest_os_kind(&self) -> anyhow::Result<GuestOsKind> {
        Ok(self
            .artifact_store
            .get_guest_os_image(&self.default_guest_os_artifact)
            .await?
            .1)
    }

    /// Indicates whether the disk factory in this framework supports the
    /// creation of Crucible disks. This can be used to skip tests that require
    /// Crucible support.
    pub fn crucible_enabled(&self) -> bool {
        self.crucible_enabled
    }

    /// Indicates whether a "migration base" Propolis server artifact is
    /// available for migration-from-base tests.
    pub fn migration_base_enabled(&self) -> bool {
        self.migration_base_enabled
    }

    /// Yields a sender to which the caller can submit tasks that will be
    /// `await`ed after the calling test case completes.
    pub(crate) fn cleanup_task_channel(
        &self,
    ) -> UnboundedSender<JoinHandle<()>> {
        self.cleanup_task_tx.clone()
    }

    /// Runs any currently-queued cleanup tasks in this `Framework` to
    /// completion.
    ///
    /// This routine synchronizes access to the cleanup task queue such that
    /// when it returns, any cleanup tasks that were previously queued by the
    /// calling thread are guaranteed to have been completed (though not
    /// necessarily by the calling thread itself, i.e., another, earlier caller
    /// may have awaited the task).
    async fn wait_for_cleanup_tasks(&self) {
        let futs = FuturesUnordered::new();

        let mut guard = self.cleanup_task_rx.lock().await;
        while let Ok(task) = guard.try_recv() {
            futs.push(task);
        }

        // Hold the lock while awaiting the tasks to block subsequent callers.
        // This is needed to guarantee that all tasks submitted by a thread are
        // retired when that thread returns from this call: without the lock, T1
        // can submit a task, T2 can remove it from the queue but not fully
        // retire it, and a subsequent call from T1 will see an empty queue and
        // return immediately even though its task is still active.
        let _results: Vec<_> = futs.collect().await;
    }
}

impl fmt::Display for CrucibleDownstairsSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BuildomatGitRev(commit) => {
                write!(f, "Buildomat Git commit '{commit}'")
            }
            Self::Local(path) => write!(f, "local path '{path}'"),
        }
    }
}

impl fmt::Display for BasePropolisSource<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BuildomatBranch(branch) => {
                write!(f, "Buildomat branch '{branch}'")
            }
            Self::BuildomatGitRev(commit) => {
                write!(f, "Buildomat Git commit '{commit}'")
            }
            Self::Local(path) => write!(f, "local path '{path}'"),
        }
    }
}
