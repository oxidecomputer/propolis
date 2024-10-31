// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Tools for handling instance ensure requests.
//!
//! To initialize a new VM, the server must (1) create a set of VM objects from
//! an instance spec, (2) set up VM services that use those objects, (3) use the
//! objects and services to drive the VM state machine to the `ActiveVm` state,
//! and (4) notify the original caller of the "instance ensure" API of the
//! completion of its request. If VM initialization fails, the actions required
//! to compensate and drive the state machine to `RundownComplete` depend on how
//! many steps were completed.
//!
//! When live migrating into an instance, the live migration task interleaves
//! initialization steps with the steps of the live migration protocol, and
//! needs to be able to unwind initialization correctly whenever the migration
//! protocol fails.
//!
//! The `VmEnsure` types in this module exist to hide the gory details of
//! initializing and unwinding from higher-level operations like the live
//! migration task. Each type represents a phase of the initialization process
//! and has a routine that consumes the current phase and moves to the next
//! phase. If a higher-level operation fails, it can call a failure handler on
//! its current phase to unwind the whole operation and drive the VM state
//! machine to the correct resting state.

use std::sync::Arc;

use oximeter::types::ProducerRegistry;
use oximeter_instruments::kstat::KstatSampler;
use propolis_api_types::{
    InstanceEnsureResponse, InstanceMigrateInitiateResponse,
    InstanceProperties, InstanceState,
};
use slog::{debug, info};

use crate::{
    initializer::{
        build_instance, MachineInitializer, MachineInitializerState,
    },
    migrate::destination::MigrationTargetInfo,
    spec::Spec,
    stats::{create_kstat_sampler, VirtualMachine},
    vm::{
        request_queue::InstanceAutoStart, VMM_BASE_RT_THREADS,
        VMM_MIN_RT_THREADS,
    },
};

use super::{
    objects::{InputVmObjects, VmObjects},
    services::VmServices,
    state_driver::InputQueue,
    state_publisher::{ExternalStateUpdate, StatePublisher},
    EnsureOptions, InstanceEnsureResponseTx, VmError,
};

pub(crate) enum VmInitializationMethod {
    Spec(Spec),
    Migration(MigrationTargetInfo),
}

pub(crate) struct VmEnsureRequest {
    pub(crate) properties: InstanceProperties,
    pub(crate) init: VmInitializationMethod,
}

impl VmEnsureRequest {
    /// Returns `true` if this is a request to initialize via live migration.
    pub(crate) fn is_migration(&self) -> bool {
        matches!(self.init, VmInitializationMethod::Migration(_))
    }

    /// Returns the migration target information if this is a request to
    /// initialize via live migration and `None` otherwise.
    pub(crate) fn migration_info(&self) -> Option<&MigrationTargetInfo> {
        match &self.init {
            VmInitializationMethod::Migration(info) => Some(info),
            VmInitializationMethod::Spec(_) => None,
        }
    }

    /// Returns the instance spec to use to initialize this VM if this is a
    /// request to initialize a VM from scratch; returns `None` otherwise.
    pub(crate) fn spec(&self) -> Option<&Spec> {
        match &self.init {
            VmInitializationMethod::Migration(_) => None,
            VmInitializationMethod::Spec(spec) => Some(spec),
        }
    }
}

/// Holds state about an instance ensure request that has not yet produced any
/// VM objects or driven the VM state machine to the `ActiveVm` state.
pub(crate) struct VmEnsureNotStarted<'a> {
    log: &'a slog::Logger,
    vm: &'a Arc<super::Vm>,
    ensure_request: &'a VmEnsureRequest,
    ensure_options: &'a Arc<EnsureOptions>,
    ensure_response_tx: InstanceEnsureResponseTx,
    state_publisher: &'a mut StatePublisher,
}

impl<'a> VmEnsureNotStarted<'a> {
    pub(super) fn new(
        log: &'a slog::Logger,
        vm: &'a Arc<super::Vm>,
        ensure_request: &'a VmEnsureRequest,
        ensure_options: &'a Arc<EnsureOptions>,
        ensure_response_tx: InstanceEnsureResponseTx,
        state_publisher: &'a mut StatePublisher,
    ) -> Self {
        Self {
            log,
            vm,
            ensure_request,
            ensure_options,
            ensure_response_tx,
            state_publisher,
        }
    }

    pub(crate) fn state_publisher(&mut self) -> &mut StatePublisher {
        self.state_publisher
    }

    pub(crate) fn migration_info(&self) -> Option<&MigrationTargetInfo> {
        self.ensure_request.migration_info()
    }

    pub(crate) async fn create_objects_for_new_vm(
        self,
    ) -> anyhow::Result<VmEnsureObjectsCreated<'a>> {
        let VmInitializationMethod::Spec(spec) = &self.ensure_request.init
        else {
            panic!("create_objects_for_new_vm requires init via explicit spec");
        };

        self.create_objects(spec.clone()).await
    }

    pub(crate) async fn create_objects_for_migration(
        self,
        spec: Spec,
    ) -> anyhow::Result<VmEnsureObjectsCreated<'a>> {
        assert!(self.ensure_request.is_migration());
        self.create_objects(spec).await
    }

    /// Creates a set of VM objects using the instance spec stored in this
    /// ensure request, but does not install them as an active VM.
    async fn create_objects(
        self,
        spec: Spec,
    ) -> anyhow::Result<VmEnsureObjectsCreated<'a>> {
        debug!(self.log, "creating VM objects");

        let input_queue = Arc::new(InputQueue::new(
            self.log.new(slog::o!("component" => "request_queue")),
            if self.ensure_request.is_migration() {
                InstanceAutoStart::Yes
            } else {
                InstanceAutoStart::No
            },
        ));

        // Create the runtime that will host tasks created by VMM components
        // (e.g. block device runtime tasks).
        let vmm_rt = tokio::runtime::Builder::new_multi_thread()
            .thread_name("tokio-rt-vmm")
            .worker_threads(usize::max(
                VMM_MIN_RT_THREADS,
                VMM_BASE_RT_THREADS + spec.board.cpus as usize,
            ))
            .enable_all()
            .build()?;

        // Run VM object creation on the new runtime so that if a component
        // calls `tokio::spawn`, the task will spawn onto the VMM runtime and
        // not the main server runtime.
        let log_for_init = self.log.clone();
        let properties = self.ensure_request.properties.clone();
        let options = self.ensure_options.clone();
        let queue_for_init = input_queue.clone();
        let init_result = vmm_rt
            .spawn(async move {
                let options = options.as_ref();
                initialize_vm_objects(
                    log_for_init,
                    spec,
                    properties,
                    options,
                    queue_for_init,
                )
                .await
            })
            .await
            .map_err(|e| {
                anyhow::anyhow!("failed to join VM object creation task: {e}")
            })?;

        match init_result {
            Ok(objects) => {
                // N.B. Once these `VmObjects` exist, it is no longer safe to
                //      call `vm_init_failed`.
                let objects = Arc::new(VmObjects::new(
                    self.log.clone(),
                    self.vm.clone(),
                    objects,
                ));

                Ok(VmEnsureObjectsCreated {
                    log: self.log,
                    vm: self.vm,
                    vmm_rt,
                    ensure_request: self.ensure_request,
                    ensure_options: self.ensure_options,
                    ensure_response_tx: self.ensure_response_tx,
                    state_publisher: self.state_publisher,
                    vm_objects: objects,
                    input_queue,
                    kernel_vm_paused: false,
                })
            }
            Err(e) => Err(self.fail(e).await),
        }
    }

    pub(crate) async fn fail(self, reason: anyhow::Error) -> anyhow::Error {
        self.state_publisher
            .update(ExternalStateUpdate::Instance(InstanceState::Failed));

        self.vm.vm_init_failed().await;
        let _ = self
            .ensure_response_tx
            .send(Err(VmError::InitializationFailed(reason.to_string())));

        reason
    }
}

/// Represents an instance ensure request that has proceeded far enough to
/// create a set of VM objects, but that has not yet installed those objects as
/// an `ActiveVm` or notified the requestor that its request is complete.
pub(crate) struct VmEnsureObjectsCreated<'a> {
    log: &'a slog::Logger,
    vm: &'a Arc<super::Vm>,
    vmm_rt: tokio::runtime::Runtime,
    ensure_request: &'a VmEnsureRequest,
    ensure_options: &'a EnsureOptions,
    ensure_response_tx: InstanceEnsureResponseTx,
    state_publisher: &'a mut StatePublisher,
    vm_objects: Arc<VmObjects>,
    input_queue: Arc<InputQueue>,
    kernel_vm_paused: bool,
}

impl<'a> VmEnsureObjectsCreated<'a> {
    /// Prepares the VM's CPUs for an incoming live migration by activating them
    /// (at the kernel VM level) and then pausing the kernel VM. This must be
    /// done before importing any state into these objects.
    ///
    /// # Panics
    ///
    /// Panics if called more than once on the same set of objects.
    pub(crate) async fn prepare_for_migration(&mut self) {
        assert!(!self.kernel_vm_paused);
        let guard = self.vm_objects.lock_exclusive().await;
        guard.reset_vcpus();
        guard.pause_kernel_vm();
        self.kernel_vm_paused = true;
    }

    /// Uses this struct's VM objects to create a set of VM services, then
    /// installs an active VM into the parent VM state machine and notifies the
    /// ensure requester that its request is complete.
    pub(crate) async fn ensure_active(self) -> VmEnsureActive<'a> {
        let vm_services = VmServices::new(
            self.log,
            &self.vm_objects,
            &self.ensure_request.properties,
            self.ensure_options,
        )
        .await;

        // The VMM runtime itself lives in the `ActiveVm` structure created by
        // this call. Preserve a handle to it to be passed back to the
        // initialization process so that it can launch the state driver task
        // onto this runtime.
        let vmm_rt_hdl = self.vmm_rt.handle().clone();
        self.vm
            .make_active(
                self.log,
                self.input_queue.clone(),
                &self.vm_objects,
                self.vmm_rt,
                vm_services,
            )
            .await;

        // The response channel may be closed if the client who asked to ensure
        // the VM timed out or disconnected. This is OK; now that the VM is
        // active, a new client can recover by reading the current instance
        // state and using the state change API to send commands to the state
        // driver.
        let _ = self.ensure_response_tx.send(Ok(InstanceEnsureResponse {
            migrate: match &self.ensure_request.init {
                VmInitializationMethod::Spec(_) => None,
                VmInitializationMethod::Migration(info) => {
                    Some(InstanceMigrateInitiateResponse {
                        migration_id: info.migration_id,
                    })
                }
            },
        }));

        VmEnsureActive {
            vm: self.vm,
            vmm_rt_hdl,
            state_publisher: self.state_publisher,
            vm_objects: self.vm_objects,
            input_queue: self.input_queue,
            kernel_vm_paused: self.kernel_vm_paused,
        }
    }
}

/// Describes a set of VM objects that are fully initialized and referred to by
/// the `ActiveVm` in a VM state machine, but for which a state driver loop has
/// not started yet.
pub(crate) struct VmEnsureActive<'a> {
    vm: &'a Arc<super::Vm>,
    vmm_rt_hdl: tokio::runtime::Handle,
    state_publisher: &'a mut StatePublisher,
    vm_objects: Arc<VmObjects>,
    input_queue: Arc<InputQueue>,
    kernel_vm_paused: bool,
}

pub(super) struct VmEnsureActiveOutput {
    pub vm_objects: Arc<VmObjects>,
    pub input_queue: Arc<InputQueue>,
    pub vmm_rt_hdl: tokio::runtime::Handle,
}

impl<'a> VmEnsureActive<'a> {
    pub(crate) fn vm_objects(&self) -> &Arc<VmObjects> {
        &self.vm_objects
    }

    pub(crate) fn state_publisher(&mut self) -> &mut StatePublisher {
        self.state_publisher
    }

    pub(crate) async fn fail(mut self) {
        // If a caller asked to prepare the VM objects for migration in the
        // previous phase, make sure that operation is undone before the VM
        // objects are torn down.
        if self.kernel_vm_paused {
            let guard = self.vm_objects.lock_exclusive().await;
            guard.resume_kernel_vm();
            self.kernel_vm_paused = false;
        }

        self.state_publisher
            .update(ExternalStateUpdate::Instance(InstanceState::Failed));

        // Since there are extant VM objects, move to the Rundown state. The VM
        // will move to RundownComplete when the objects are finally dropped.
        self.vm.set_rundown().await;
    }

    /// Yields the VM objects and input queue for this VM so that they can be
    /// used to start a state driver loop.
    pub(super) fn into_inner(self) -> VmEnsureActiveOutput {
        VmEnsureActiveOutput {
            vm_objects: self.vm_objects,
            input_queue: self.input_queue,
            vmm_rt_hdl: self.vmm_rt_hdl,
        }
    }
}

/// Initializes a set of VM objects from the supplied specification and options.
///
/// This function should be called from the VMM runtime. This ensures that if
/// the new VM objects create tokio tasks, they will run on the VMM runtime and
/// not the Dropshot server runtime.
async fn initialize_vm_objects(
    log: slog::Logger,
    spec: Spec,
    properties: InstanceProperties,
    options: &EnsureOptions,
    event_queue: Arc<InputQueue>,
) -> anyhow::Result<InputVmObjects> {
    info!(log, "initializing new VM";
          "spec" => #?spec,
          "properties" => #?properties,
          "use_reservoir" => options.use_reservoir,
          "bootrom" => %options.toml_config.bootrom.display());

    let vmm_log = log.new(slog::o!("component" => "vmm"));

    // Set up the 'shell' instance into which the rest of this routine will
    // add components.
    let machine = build_instance(
        &properties.vm_name(),
        &spec,
        options.use_reservoir,
        vmm_log,
    )?;

    let mut init = MachineInitializer {
        log: log.clone(),
        machine: &machine,
        devices: Default::default(),
        block_backends: Default::default(),
        crucible_backends: Default::default(),
        spec: &spec,
        properties: &properties,
        toml_config: &options.toml_config,
        producer_registry: options.oximeter_registry.clone(),
        state: MachineInitializerState::default(),
        kstat_sampler: initialize_kstat_sampler(
            &log,
            &spec,
            options.oximeter_registry.clone(),
        ),
        stats_vm: VirtualMachine::new(spec.board.cpus, &properties),
    };

    init.initialize_rom(options.toml_config.bootrom.as_path())?;
    let chipset = init.initialize_chipset(
        &(event_queue.clone()
            as Arc<dyn super::guest_event::ChipsetEventHandler>),
    )?;

    init.initialize_rtc(&chipset)?;
    init.initialize_hpet();

    let com1 = Arc::new(init.initialize_uart(&chipset));
    let ps2ctrl = init.initialize_ps2(&chipset);
    init.initialize_qemu_debug_port()?;
    init.initialize_qemu_pvpanic(VirtualMachine::new(
        spec.board.cpus,
        &properties,
    ))?;
    init.initialize_network_devices(&chipset).await?;

    #[cfg(not(feature = "omicron-build"))]
    init.initialize_test_devices(&options.toml_config.devices);
    #[cfg(feature = "omicron-build")]
    info!(log, "`omicron-build` feature enabled, ignoring any test devices");

    #[cfg(feature = "falcon")]
    {
        init.initialize_softnpu_ports(&chipset)?;
        init.initialize_9pfs(&chipset);
    }

    init.initialize_storage_devices(&chipset, options.nexus_client.clone())
        .await?;

    let ramfb = init.initialize_fwcfg(spec.board.cpus)?;
    init.initialize_cpus().await?;
    let vcpu_tasks = Box::new(crate::vcpu_tasks::VcpuTasks::new(
        &machine,
        event_queue.clone() as Arc<dyn super::guest_event::VcpuEventHandler>,
        log.new(slog::o!("component" => "vcpu_tasks")),
    )?);

    let MachineInitializer {
        devices, block_backends, crucible_backends, ..
    } = init;

    Ok(InputVmObjects {
        instance_spec: spec,
        vcpu_tasks,
        machine,
        devices,
        block_backends,
        crucible_backends,
        com1,
        framebuffer: Some(ramfb),
        ps2ctrl,
    })
}

/// Create an object used to sample kstats.
fn initialize_kstat_sampler(
    log: &slog::Logger,
    spec: &Spec,
    producer_registry: Option<ProducerRegistry>,
) -> Option<KstatSampler> {
    let registry = producer_registry?;
    let sampler = create_kstat_sampler(log, spec)?;

    match registry.register_producer(sampler.clone()) {
        Ok(_) => Some(sampler),
        Err(e) => {
            slog::error!(
                log,
                "Failed to register kstat sampler in producer \
                registry, no kstat-based metrics will be produced";
                "error" => ?e,
            );
            None
        }
    }
}
