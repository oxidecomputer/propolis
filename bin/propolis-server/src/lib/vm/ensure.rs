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
    InstanceEnsureResponse, InstanceMigrateInitiateRequest,
    InstanceMigrateInitiateResponse, InstanceProperties, InstanceState,
};
use slog::{debug, info};

use crate::{
    initializer::{
        build_instance, MachineInitializer, MachineInitializerState,
    },
    spec::Spec,
    stats::create_kstat_sampler,
    vm::request_queue::InstanceAutoStart,
};

use super::{
    objects::{InputVmObjects, VmObjects},
    services::VmServices,
    state_driver::InputQueue,
    state_publisher::{ExternalStateUpdate, StatePublisher},
    EnsureOptions, InstanceEnsureResponseTx, VmError,
};

pub(crate) struct VmEnsureRequest {
    pub(crate) properties: InstanceProperties,
    pub(crate) migrate: Option<InstanceMigrateInitiateRequest>,
    pub(crate) instance_spec: Spec,
}

/// Holds state about an instance ensure request that has not yet produced any
/// VM objects or driven the VM state machine to the `ActiveVm` state.
pub(crate) struct VmEnsureNotStarted<'a> {
    log: &'a slog::Logger,
    vm: &'a Arc<super::Vm>,
    ensure_request: &'a VmEnsureRequest,
    ensure_options: &'a EnsureOptions,
    ensure_response_tx: InstanceEnsureResponseTx,
    state_publisher: &'a mut StatePublisher,
}

impl<'a> VmEnsureNotStarted<'a> {
    pub(super) fn new(
        log: &'a slog::Logger,
        vm: &'a Arc<super::Vm>,
        ensure_request: &'a VmEnsureRequest,
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

    pub(crate) fn instance_spec(&self) -> &Spec {
        &self.ensure_request.instance_spec
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

        match self.initialize_vm_objects_from_spec(&input_queue).await {
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

    async fn initialize_vm_objects_from_spec(
        &self,
        event_queue: &Arc<InputQueue>,
    ) -> anyhow::Result<InputVmObjects> {
        let properties = &self.ensure_request.properties;
        let spec = &self.ensure_request.instance_spec;
        let options = self.ensure_options;

        info!(self.log, "initializing new VM";
              "spec" => #?spec,
              "properties" => #?properties,
              "use_reservoir" => options.use_reservoir,
              "bootrom" => %options.toml_config.bootrom.display());

        let vmm_log = self.log.new(slog::o!("component" => "vmm"));

        // Set up the 'shell' instance into which the rest of this routine will
        // add components.
        let machine = build_instance(
            &properties.vm_name(),
            spec,
            options.use_reservoir,
            vmm_log,
        )?;

        let mut init = MachineInitializer {
            log: self.log.clone(),
            machine: &machine,
            devices: Default::default(),
            block_backends: Default::default(),
            crucible_backends: Default::default(),
            spec,
            properties,
            toml_config: &options.toml_config,
            producer_registry: options.oximeter_registry.clone(),
            state: MachineInitializerState::default(),
            kstat_sampler: initialize_kstat_sampler(
                self.log,
                properties,
                self.instance_spec(),
                options.oximeter_registry.clone(),
            ),
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
        init.initialize_network_devices(&chipset).await?;

        #[cfg(not(feature = "omicron-build"))]
        init.initialize_test_devices(&options.toml_config.devices)?;
        #[cfg(feature = "omicron-build")]
        info!(
            self.log,
            "`omicron-build` feature enabled, ignoring any test devices"
        );

        #[cfg(feature = "falcon")]
        {
            init.initialize_softnpu_ports(&chipset)?;
            init.initialize_9pfs(&chipset)?;
        }

        init.initialize_storage_devices(&chipset, options.nexus_client.clone())
            .await?;

        let ramfb = init.initialize_fwcfg(self.instance_spec().board.cpus)?;
        init.initialize_cpus().await?;
        let vcpu_tasks = Box::new(crate::vcpu_tasks::VcpuTasks::new(
            &machine,
            event_queue.clone()
                as Arc<dyn super::guest_event::VcpuEventHandler>,
            self.log.new(slog::o!("component" => "vcpu_tasks")),
        )?);

        let MachineInitializer {
            devices,
            block_backends,
            crucible_backends,
            ..
        } = init;

        Ok(InputVmObjects {
            instance_spec: spec.clone(),
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
}

/// Represents an instance ensure request that has proceeded far enough to
/// create a set of VM objects, but that has not yet installed those objects as
/// an `ActiveVm` or notified the requestor that its request is complete.
pub(crate) struct VmEnsureObjectsCreated<'a> {
    log: &'a slog::Logger,
    vm: &'a Arc<super::Vm>,
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

        self.vm
            .make_active(
                self.log,
                self.input_queue.clone(),
                &self.vm_objects,
                vm_services,
            )
            .await;

        // The response channel may be closed if the client who asked to ensure
        // the VM timed out or disconnected. This is OK; now that the VM is
        // active, a new client can recover by reading the current instance
        // state and using the state change API to send commands to the state
        // driver.
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

/// Create an object used to sample kstats.
fn initialize_kstat_sampler(
    log: &slog::Logger,
    properties: &InstanceProperties,
    spec: &Spec,
    producer_registry: Option<ProducerRegistry>,
) -> Option<KstatSampler> {
    let registry = producer_registry?;
    let sampler = create_kstat_sampler(log, properties, spec)?;

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
