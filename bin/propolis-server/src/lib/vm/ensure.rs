// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Tools for handling instance ensure requests.
//!
//! The types in this module aim to help contain two bits of complexity:
//!
//! 1. When a request to create a Propolis instance fails, lots of things have
//!    to happen:
//!    - The externally-visible instance state must move to Failed.
//!    - The ensure request itself must return a failure status.
//!    - The VM state machine must move into a rundown state--but which one
//!      depends on whether instance creation got far enough to create VM
//!      objects: if it did, the state machine must go to Rundown and wait for
//!      the objects to be destroyed; if it didn't, the instance goes straight
//!      to RundownComplete.
//! 2. When initializing via live migration, the steps needed to initialize an
//!    instance are interleaved with the live migration's phases: the ensure
//!    request returns as soon as the VM's objects are created, but the
//!    migration itself must complete before the VM enters its main state driver
//!    loop.
//!
//! The types in this module describe three different phases of instance
//! initialization:
//!
//! 1. Not started: the VM state machine exists and an ensure request has been
//!    received, but no VM objects have been created yet.
//! 2. Objects created: a set of VM objects has been created, but no active
//!    VM has yet been installed in the VM state machine.
//! 3. VM activated: a set of VM objects has been turned into an `ActiveVm` and
//!    installed in the VM state machine.
//!
//! Each phase type has a transition function that consumes the phase and tries
//! to transition to the next phase. Each type also has an explicit `fail`
//! method that allows an external caller (e.g. the live migration procedure) to
//! indicate that a higher-level failure occurred and that the phase should run
//! its cleanup code.

use std::sync::Arc;

use propolis_api_types::{
    instance_spec::{v0::InstanceSpecV0, VersionedInstanceSpec},
    InstanceEnsureResponse, InstanceMigrateInitiateResponse,
    InstanceProperties, InstanceSpecEnsureRequest, InstanceState,
};
use slog::{debug, info};

use crate::{
    initializer::{
        build_instance, MachineInitializer, MachineInitializerState,
    },
    vm::request_queue::InstanceAutoStart,
};

use super::{
    objects::{InputVmObjects, VmObjects},
    services::VmServices,
    state_driver::InputQueue,
    state_publisher::{ExternalStateUpdate, StatePublisher},
    EnsureOptions, InstanceEnsureResponseTx, VmError,
};

/// Holds state about an instance ensure request that has not yet produced any
/// VM objects or driven the VM state machine to the `ActiveVm` state.
pub(crate) struct VmEnsureNotStarted<'a> {
    log: &'a slog::Logger,
    vm: &'a Arc<super::Vm>,
    ensure_request: &'a InstanceSpecEnsureRequest,
    ensure_options: &'a EnsureOptions,
    ensure_response_tx: InstanceEnsureResponseTx,
    state_publisher: &'a mut StatePublisher,
}

impl<'a> VmEnsureNotStarted<'a> {
    pub(super) fn new(
        log: &'a slog::Logger,
        vm: &'a Arc<super::Vm>,
        ensure_request: &'a InstanceSpecEnsureRequest,
        ensure_options: &'a EnsureOptions,
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

    pub(crate) fn instance_spec(&self) -> &InstanceSpecV0 {
        let VersionedInstanceSpec::V0(v0) = &self.ensure_request.instance_spec;
        v0
    }

    pub(crate) fn state_publisher(&mut self) -> &mut StatePublisher {
        self.state_publisher
    }

    /// Creates a set of VM objects using the instance spec stored in this
    /// ensure request, but does not install them as an active VM.
    pub(crate) async fn create_objects(
        self,
    ) -> anyhow::Result<VmEnsureObjectsCreated<'a>> {
        debug!(self.log, "creating VM objects");

        let input_queue = Arc::new(InputQueue::new(
            self.log.new(slog::o!("component" => "request_queue")),
            match &self.ensure_request.migrate {
                Some(_) => InstanceAutoStart::Yes,
                None => InstanceAutoStart::No,
            },
        ));

        match initialize_vm_objects_from_spec(
            self.log,
            &input_queue,
            &self.ensure_request.properties,
            &self.ensure_request.instance_spec,
            self.ensure_options,
        )
        .await
        {
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
    ensure_request: &'a InstanceSpecEnsureRequest,
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
            self.vm,
            &self.vm_objects,
            &self.ensure_request.properties,
            self.ensure_options,
        )
        .await;

        self.vm
            .make_active(
                self.log,
                self.input_queue.clone(),
                &self.vm_objects,
                vm_services,
            )
            .await;

        let _ = self.ensure_response_tx.send(Ok(InstanceEnsureResponse {
            migrate: self.ensure_request.migrate.as_ref().map(|req| {
                InstanceMigrateInitiateResponse {
                    migration_id: req.migration_id,
                }
            }),
        }));

        VmEnsureActive {
            vm: self.vm,
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
    state_publisher: &'a mut StatePublisher,
    vm_objects: Arc<VmObjects>,
    input_queue: Arc<InputQueue>,
    kernel_vm_paused: bool,
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
    pub(super) fn into_inner(self) -> (Arc<VmObjects>, Arc<InputQueue>) {
        (self.vm_objects, self.input_queue)
    }
}

/// Initializes a set of Propolis components from the supplied instance spec.
async fn initialize_vm_objects_from_spec(
    log: &slog::Logger,
    event_queue: &Arc<InputQueue>,
    properties: &InstanceProperties,
    spec: &VersionedInstanceSpec,
    options: &EnsureOptions,
) -> anyhow::Result<InputVmObjects> {
    info!(log, "initializing new VM";
              "spec" => #?spec,
              "properties" => #?properties,
              "use_reservoir" => options.use_reservoir,
              "bootrom" => %options.toml_config.bootrom.display());

    let vmm_log = log.new(slog::o!("component" => "vmm"));

    // Set up the 'shell' instance into which the rest of this routine will
    // add components.
    let VersionedInstanceSpec::V0(v0_spec) = &spec;
    let machine = build_instance(
        &properties.vm_name(),
        v0_spec,
        options.use_reservoir,
        vmm_log,
    )?;

    let mut init = MachineInitializer {
        log: log.clone(),
        machine: &machine,
        devices: Default::default(),
        block_backends: Default::default(),
        crucible_backends: Default::default(),
        spec: v0_spec,
        properties,
        toml_config: &options.toml_config,
        producer_registry: options.oximeter_registry.clone(),
        state: MachineInitializerState::default(),
    };

    init.initialize_rom(options.toml_config.bootrom.as_path())?;
    let chipset = init.initialize_chipset(
        &(event_queue.clone()
            as Arc<dyn super::guest_event::ChipsetEventHandler>),
    )?;

    init.initialize_rtc(&chipset)?;
    init.initialize_hpet()?;

    let com1 = Arc::new(init.initialize_uart(&chipset)?);
    let ps2ctrl = init.initialize_ps2(&chipset)?;
    init.initialize_qemu_debug_port()?;
    init.initialize_qemu_pvpanic(properties.into())?;
    init.initialize_network_devices(&chipset)?;

    #[cfg(not(feature = "omicron-build"))]
    init.initialize_test_devices(&options.toml_config.devices)?;
    #[cfg(feature = "omicron-build")]
    info!(log, "`omicron-build` feature enabled, ignoring any test devices");

    #[cfg(feature = "falcon")]
    init.initialize_softnpu_ports(&chipset)?;
    #[cfg(feature = "falcon")]
    init.initialize_9pfs(&chipset)?;

    init.initialize_storage_devices(&chipset, options.nexus_client.clone())
        .await?;

    let ramfb = init.initialize_fwcfg(v0_spec.devices.board.cpus)?;
    init.initialize_cpus()?;
    let vcpu_tasks = Box::new(crate::vcpu_tasks::VcpuTasks::new(
        &machine,
        event_queue.clone() as Arc<dyn super::guest_event::VcpuEventHandler>,
        log.new(slog::o!("component" => "vcpu_tasks")),
    )?);

    let MachineInitializer {
        devices, block_backends, crucible_backends, ..
    } = init;

    Ok(InputVmObjects {
        instance_spec: v0_spec.clone(),
        vcpu_tasks,
        machine,
        lifecycle_components: devices,
        block_backends,
        crucible_backends,
        com1,
        framebuffer: Some(ramfb),
        ps2ctrl,
    })
}
