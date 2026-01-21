// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Tools for handling instance ensure requests.
//!
//! This module handles the first high-level phase of a VM's lifecycle, which
//! creates all of the VM's components and attendant data structures. These are
//! handed off to a `StateDriver` that implements the main VM event loop. See
//! the [`state_driver`] module docs for more details.
//!
//! This module uses distinct structs that each represent a distinct phase of VM
//! initialization. When a server receives a new ensure request, it creates the
//! first of these structures, then hands it off to the procedure described in
//! the ensure request to drive the rest of the initialization process, as in
//! the diagram below:
//!
//! ```text
//!                 +-------------------------+
//!                 |                         |
//!                 |  Initial state (no VM)  |
//!                 |                         |
//!                 +-----------+-------------+
//!                             |
//!                    Receive ensure request
//!                             |
//!                             v
//!                     VmEnsureNotStarted
//!                             |
//!                             |
//!                   +---------v----------+
//!          Yes      |                    |        No
//!           +-------+  Live migration?   +---------+
//!           |       |                    |         |
//!           |       +--------------------+         |
//!           |                                      |
//!     +-----v------+                               |
//!     |Get params  |                               |
//!     |from source |                               |
//!     +-----+------+                               |
//!           |                                      |
//!     +-----v------+                     +---------v-----------+
//!     |Initialize  |                     |Initialize components|
//!     |components  |                     |    from params      |
//!     +-----+------+                     +---------+-----------+
//!           |                                      |
//!           v                                      v
//! VmEnsureObjectsCreated                 VmEnsureObjectsCreated
//!           |                                      |
//!           |                                      |
//!     +-----v------+                               |
//!     |Import state|                               |
//!     |from source |                               |
//!     +-----+------+                               |
//!           |                                      |
//!           |                                      |
//!           |        +------------------+          |
//!           +-------->Launch VM services<----------+
//!                    +--------+---------+
//!                             |
//!                             |
//!                    +--------v---------+
//!                    |Move VM to Active |
//!                    +--------+---------+
//!                             |
//!                             |
//!                             v
//!                      VmEnsureActive<'_>
//! ```
//!
//! When initializing a VM from scratch, the ensure request contains a spec that
//! determines what components the VM should create, and they are created into
//! their default initial states. When migrating in, the VM-ensure structs are
//! handed off to the migration protocol, which fetches a spec from the
//! migration source, uses its contents to create the VM's components, and
//! imports the source VM's device state into those components.
//!
//! Once all components exist and are initialized, this module sets up "VM
//! services" (e.g. the serial console and metrics) that connect this VM to
//! other Oxide APIs and services. It then updates the server's VM state machine
//! and yields a completed "active" VM that can be passed into a state driver
//! run loop.
//!
//! Separating the initialization steps in this manner hides the gory details of
//! initializing a VM (and unwinding initialization) from higher-level
//! procedures like the migration protocol. Each initialize phase has a failure
//! handler that allows a higher-level driver to unwind the entire ensure
//! operation and drive the VM state machine to the correct resting state.
//!
//! [`state_driver`]: crate::vm::state_driver

use std::sync::Arc;

use oximeter::types::ProducerRegistry;
use oximeter_instruments::kstat::KstatSampler;
use propolis::enlightenment::{
    bhyve::BhyveGuestInterface,
    hyperv::{Features as HyperVFeatures, HyperV},
    Enlightenment,
};
use propolis_api_types::instance::{
    InstanceEnsureResponse, InstanceProperties, InstanceState,
};
use propolis_api_types::instance_spec::components::board::{
    GuestHypervisorInterface, HyperVFeatureFlag,
};
use propolis_api_types::migration::InstanceMigrateInitiateResponse;
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
    Spec(Box<Spec>),
    Migration(MigrationTargetInfo),
}

pub(crate) struct VmEnsureRequest {
    pub(crate) properties: InstanceProperties,
    pub(crate) init: VmInitializationMethod,
}

impl VmEnsureRequest {
    pub(crate) fn is_migration(&self) -> bool {
        matches!(self.init, VmInitializationMethod::Migration(_))
    }

    pub(crate) fn migration_info(&self) -> Option<&MigrationTargetInfo> {
        match &self.init {
            VmInitializationMethod::Spec(_) => None,
            VmInitializationMethod::Migration(info) => Some(info),
        }
    }

    pub(crate) fn spec(&self) -> Option<&Spec> {
        match &self.init {
            VmInitializationMethod::Spec(spec) => Some(spec),
            VmInitializationMethod::Migration(_) => None,
        }
    }
}

/// Holds state about an instance ensure request that has not yet produced any
/// VM objects or driven the VM state machine to the `ActiveVm` state.
pub(crate) struct VmEnsureNotStarted<'a> {
    log: &'a slog::Logger,
    vm: &'a Arc<super::Vm>,
    ensure_request: &'a VmEnsureRequest,

    // VM objects are created on a separate tokio task from the one that drives
    // the instance ensure state machine. This task needs its own copy of the
    // ensure options. `EnsureOptions` is not `Clone`, so take a reference to an
    // `Arc` wrapper around the options to have something that can be cloned and
    // passed to the ensure task.
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

    pub(crate) async fn create_objects_from_request(
        self,
    ) -> anyhow::Result<VmEnsureObjectsCreated<'a>> {
        let spec = self
            .ensure_request
            .spec()
            .expect(
                "create_objects_from_request is called with an explicit spec",
            )
            .clone();

        self.create_objects(spec).await
    }

    pub(crate) async fn create_objects_from_spec(
        self,
        spec: Spec,
    ) -> anyhow::Result<VmEnsureObjectsCreated<'a>> {
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

        let log_for_init = self.log.clone();
        let properties = self.ensure_request.properties.clone();
        let options = self.ensure_options.clone();
        let queue_for_init = input_queue.clone();

        // Either the block following this succeeds with both a Tokio runtime
        // and VM objects, or entirely fails with no partial state for us to
        // clean up.
        type InitResult =
            anyhow::Result<(tokio::runtime::Runtime, InputVmObjects)>;

        // We need to create a new runtime to host the tasks for this VMM's
        // objects, but that initialization is fallible and results in dropping
        // the fledgling VMM runtime itself. Dropping a Tokio runtime on a
        // worker thread in a Tokio runtime will panic, so do all init in a
        // `spawn_blocking` where this won't be an issue.
        //
        // When the runtime is returned to this thread, it must not be dropped.
        // That means that the path between this result and returning an
        // `Ok(VmEnsureObjectsCreated)` must be infallible.
        let result: InitResult = tokio::task::spawn_blocking(move || {
            // Create the runtime that will host tasks created by
            // VMM components (e.g. block device runtime tasks).
            let vmm_rt = {
                let mut builder = tokio::runtime::Builder::new_multi_thread();
                builder.thread_name("tokio-rt-vmm").worker_threads(usize::max(
                    VMM_MIN_RT_THREADS,
                    VMM_BASE_RT_THREADS + spec.board.cpus as usize,
                ));
                oxide_tokio_rt::build(&mut builder)?
            };

            let init_result = vmm_rt
                .block_on(async move {
                    initialize_vm_objects(
                        log_for_init,
                        spec,
                        properties,
                        options,
                        queue_for_init,
                    )
                    .await
                })
                .map_err(|e| {
                    anyhow::anyhow!(
                        "failed to join VM object creation task: {e}"
                    )
                })?;
            Ok((vmm_rt, init_result))
        })
        .await
        .map_err(|e| {
            // This is extremely unexpected: if the join failed, the init
            // task panicked or was cancelled. If the init itself failed,
            // which is somewhat more reasonable, we would expect the join
            // to succeed and have an error below.
            anyhow::anyhow!("failed to join VMM runtime init task: {e}")
        })?;

        match result {
            Ok((vmm_rt, objects)) => {
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
///
/// WARNING: dropping `VmEnsureObjectsCreated` is a panic risk since dropping
/// the contained `tokio::runtime::Runtime` on in a worker thread will panic. It
/// is probably a bug to drop `VmEnsureObjectsCreated`, as it is expected users
/// will quickly call [`VmEnsureObjectsCreated::ensure_active`], but if you
/// must, take care in handling the contained `vmm_rt`.
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

        let vmm_rt_hdl = self.vmm_rt.handle().clone();
        self.vm
            .make_active(
                self.log,
                self.input_queue.clone(),
                &self.vm_objects,
                vm_services,
                self.vmm_rt,
            )
            .await;

        // The response channel may be closed if the client who asked to ensure
        // the VM timed out or disconnected. This is OK; now that the VM is
        // active, a new client can recover by reading the current instance
        // state and using the state change API to send commands to the state
        // driver.
        let _ = self.ensure_response_tx.send(Ok(InstanceEnsureResponse {
            migrate: self.ensure_request.migration_info().map(|req| {
                InstanceMigrateInitiateResponse {
                    migration_id: req.migration_id,
                }
            }),
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

impl VmEnsureActive<'_> {
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

async fn initialize_vm_objects(
    log: slog::Logger,
    spec: Spec,
    properties: InstanceProperties,
    options: Arc<EnsureOptions>,
    event_queue: Arc<InputQueue>,
) -> anyhow::Result<InputVmObjects> {
    info!(log, "initializing new VM";
              "spec" => #?spec,
              "properties" => #?properties,
              "use_reservoir" => options.use_reservoir,
              "bootrom" => %options.bootrom_path.display());

    let vmm_log = log.new(slog::o!("component" => "vmm"));

    let (guest_hv_interface, guest_hv_lifecycle) =
        match &spec.board.guest_hv_interface {
            GuestHypervisorInterface::Bhyve => {
                let bhyve = Arc::new(BhyveGuestInterface);
                let lifecycle = bhyve.clone();
                (bhyve as Arc<dyn Enlightenment>, lifecycle.as_lifecycle())
            }
            GuestHypervisorInterface::HyperV { features } => {
                let mut hv_features = HyperVFeatures::default();
                for f in features {
                    match f {
                        HyperVFeatureFlag::ReferenceTsc => {
                            hv_features.reference_tsc = true
                        }
                    }
                }

                let hyperv = Arc::new(HyperV::new(&vmm_log, hv_features));
                let lifecycle = hyperv.clone();
                (hyperv as Arc<dyn Enlightenment>, lifecycle.as_lifecycle())
            }
        };

    // Set up the 'shell' instance into which the rest of this routine will
    // add components.
    let machine = build_instance(
        &properties.vm_name(),
        &spec,
        options.use_reservoir,
        guest_hv_interface.clone(),
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
        producer_registry: options.oximeter_registry.clone(),
        state: MachineInitializerState::default(),
        kstat_sampler: initialize_kstat_sampler(
            &log,
            &spec,
            options.oximeter_registry.clone(),
        ),
        stats_vm: VirtualMachine::new(spec.board.cpus, &properties),
    };

    init.initialize_rom(options.bootrom_path.as_path())?;
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

    #[cfg(feature = "failure-injection")]
    init.initialize_test_devices();

    #[cfg(feature = "falcon")]
    {
        init.initialize_softnpu_ports(&chipset)?;
        init.initialize_9pfs(&chipset);
    }

    init.initialize_storage_devices(&chipset, options.nexus_client.clone())
        .await?;

    let ramfb =
        init.initialize_fwcfg(spec.board.cpus, &options.bootrom_version)?;

    init.register_guest_hv_interface(guest_hv_lifecycle);
    init.initialize_cpus().await?;

    let total_cpus = pbind::online_cpus()?;
    let vcpu_count: i32 = machine
        .vcpus
        .len()
        .try_into()
        .map_err(|_| anyhow::anyhow!("more than 2^31 vCPUs"))?;

    // When a VM has I/O-heavy workloads across many cores, vCPUs can end up
    // moving across the host and come with wasted time in juggling cyclics.
    //
    // Nexus can't yet determine CPU binding assignments in a central manner. In
    // lieu of it, knowing that Nexus won't oversubscribe a host we can
    // autonomously follow a CPU pinning strategy as: "if the VM is larger than
    // half of the sled, there can only be one, so bind it to specific CPUs and
    // rely on the OS to sort out the rest". This gets the largest VMs to fixed
    // vCPU->CPU assignments, which also are most likely to benefit.
    let cpu_threshold = total_cpus / 2;
    let bind_cpus = if vcpu_count > cpu_threshold {
        if vcpu_count > total_cpus {
            anyhow::bail!("spec requested more CPUs than are online!");
        }

        // Bind to the upper range of CPUs, fairly arbitrary.
        let first_bind_cpu = total_cpus - vcpu_count;
        let bind_cpus = (first_bind_cpu..total_cpus).collect();

        info!(log, "applying automatic vCPU->CPU binding";
                  "vcpu_count" => vcpu_count,
                  "total_cpus" => total_cpus,
                  "threshold" => cpu_threshold,
                  "vcpu_cpus" => #?bind_cpus);

        Some(bind_cpus)
    } else {
        None
    };

    let vcpu_tasks = Box::new(crate::vcpu_tasks::VcpuTasks::new(
        &machine,
        event_queue.clone() as Arc<dyn super::guest_event::VcpuEventHandler>,
        bind_cpus,
        log.new(slog::o!("component" => "vcpu_tasks")),
    )?);

    let MachineInitializer {
        devices, block_backends, crucible_backends, ..
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
