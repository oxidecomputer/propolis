// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Implements the VM controller: the public interface to a single Propolis
//! instance.
//!
//! The VM controller serves two purposes. First, it collects all of the objects
//! describing a single Propolis VM (the Propolis `Instance` itself, the
//! instance's spec, direct references to components in the instance, etc.).
//! Second, it records requests and events that affect how a VM moves through
//! the stages of its lifecycle, i.e. how and when it boots, reboots, migrates,
//! and stops.
//!
//! Each VM controller has a single "state driver" thread that processes
//! requests and events recorded by its controller and acts on the underlying
//! Propolis instance to move the VM into the appropriate states. Doing this
//! work on a single thread ensures that a VM can only undergo one state change
//! at a time, that there are no races to start/pause/resume/halt a VM's
//! components, and that there is a single source of truth as to a VM's current
//! state (and as to the steps that are required to move it to a different
//! state). Operations like live migration that require components to pause and
//! resume coordinate directly with the state driver thread.
//!
//! The VM controller's public API allows a Propolis Dropshot server to query a
//! VM's current state, to ask to change that state, and to obtain references to
//! objects in a VM as needed to handle other requests made of the server (e.g.
//! requests to connect to an instance's serial console or to take a disk
//! snapshot). The controller also implements traits that allow a VM's
//! components to raise events for the state driver to process (e.g. a request
//! from a VM's chipset to reboot or halt the VM).

use futures::{future::BoxFuture, stream::FuturesUnordered, StreamExt};
use std::{
    collections::{BTreeMap, VecDeque},
    fmt::Debug,
    net::SocketAddr,
    pin::Pin,
    sync::{Arc, Condvar, Mutex, Weak},
    task::{Context, Poll},
    thread::JoinHandle,
    time::Duration,
};

use oximeter::types::ProducerRegistry;
use propolis::{
    hw::{ps2::ctrl::PS2Ctrl, qemu::ramfb::RamFb, uart::LpcUart},
    vmm::Machine,
};
use propolis_api_types::{
    instance_spec::VersionedInstanceSpec, InstanceProperties,
    InstanceState as ApiInstanceState,
    InstanceStateMonitorResponse as ApiMonitoredState,
    InstanceStateRequested as ApiInstanceStateRequested,
    MigrationState as ApiMigrationState,
};
use slog::{debug, error, info, Logger};
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::oneshot;
use tokio_tungstenite::WebSocketStream;
use uuid::Uuid;

use crate::{
    initializer::{build_instance, MachineInitializer},
    migrate::MigrateError,
    serial::Serial,
    server::{BlockBackendMap, CrucibleBackendMap, DeviceMap, StaticConfig},
    vm::request_queue::ExternalRequest,
};

use self::request_queue::{ExternalRequestQueue, RequestDeniedReason};
pub use nexus_client::Client as NexusClient;

mod request_queue;
mod state_driver;

#[derive(Debug, Error)]
pub enum VmControllerError {
    #[error("The requested operation requires an active instance")]
    InstanceNotActive,

    #[error("The instance has a pending request to halt")]
    InstanceHaltPending,

    #[error("Instance is already a migration source")]
    AlreadyMigrationSource,

    #[error("Cannot request state {0:?} while migration is in progress")]
    InvalidRequestForMigrationSource(ApiInstanceStateRequested),

    #[error("A migration into this instance is in progress")]
    MigrationTargetInProgress,

    #[error("Another live migration into this instance already occurred")]
    MigrationTargetPreviouslyCompleted,

    #[error("The most recent attempt to migrate into this instance failed")]
    MigrationTargetFailed,

    #[error("Can't migrate into a running instance")]
    TooLateToBeMigrationTarget,

    #[error("Failed to queue requested state change: {0}")]
    StateChangeRequestDenied(#[from] request_queue::RequestDeniedReason),

    #[error("Migration protocol error: {0:?}")]
    MigrationProtocolError(#[from] MigrateError),

    #[error("Failed to start vCPU workers")]
    VcpuWorkerCreationFailed(#[from] super::vcpu_tasks::VcpuTaskError),

    #[error("Failed to create state worker: {0}")]
    StateWorkerCreationFailed(std::io::Error),
}

impl From<VmControllerError> for dropshot::HttpError {
    fn from(vm_error: VmControllerError) -> Self {
        use dropshot::HttpError;
        match vm_error {
            VmControllerError::AlreadyMigrationSource
            | VmControllerError::InvalidRequestForMigrationSource(_)
            | VmControllerError::MigrationTargetInProgress
            | VmControllerError::MigrationTargetFailed
            | VmControllerError::TooLateToBeMigrationTarget
            | VmControllerError::StateChangeRequestDenied(_)
            | VmControllerError::InstanceNotActive
            | VmControllerError::InstanceHaltPending
            | VmControllerError::MigrationTargetPreviouslyCompleted => {
                HttpError::for_status(
                    Some(format!("Instance operation failed: {}", vm_error)),
                    http::status::StatusCode::FORBIDDEN,
                )
            }
            VmControllerError::MigrationProtocolError(_)
            | VmControllerError::VcpuWorkerCreationFailed(_)
            | VmControllerError::StateWorkerCreationFailed(_) => {
                HttpError::for_internal_error(format!(
                    "Instance operation failed: {}",
                    vm_error
                ))
            }
        }
    }
}

/// A collection of objects that describe an instance and references to that
/// instance and its components.
pub(crate) struct VmObjects {
    /// The underlying Propolis `Machine` this controller is managing.
    machine: Option<Machine>,

    /// The instance properties supplied when this controller was created.
    properties: InstanceProperties,

    /// The instance spec used to create this controller's VM.
    spec: tokio::sync::Mutex<VersionedInstanceSpec>,

    /// Map of the emulated devices associated with the VM
    devices: DeviceMap,

    /// Map of the instance's active block backends.
    block_backends: BlockBackendMap,

    /// Map of the instance's active Crucible backends.
    crucible_backends: CrucibleBackendMap,

    /// A wrapper around the instance's first COM port, suitable for providing a
    /// connection to a guest's serial console.
    com1: Arc<Serial<LpcUart>>,

    /// An optional reference to the guest's framebuffer.
    framebuffer: Option<Arc<RamFb>>,

    /// A reference to the guest's PS/2 controller.
    ps2ctrl: Arc<PS2Ctrl>,

    /// A notification receiver to which the state worker publishes the most
    /// recent instance state information.
    monitor_rx: tokio::sync::watch::Receiver<ApiMonitoredState>,
}

/// A message sent from a live migration destination task to update the
/// externally visible state of the migration attempt.
#[derive(Clone, Copy, Debug)]
pub enum MigrateTargetCommand {
    /// Update the externally-visible migration state.
    UpdateState(ApiMigrationState),
}

/// A message sent from a live migration driver to the state worker, asking it
/// to act on source instance components on the task's behalf.
#[derive(Clone, Copy, Debug)]
pub enum MigrateSourceCommand {
    /// Update the externally-visible migration state.
    UpdateState(ApiMigrationState),

    /// Pause the instance's devices and CPUs.
    Pause,
}

/// A message sent from the state worker to the live migration driver in
/// response to a previous command.
#[derive(Debug)]
pub enum MigrateSourceResponse {
    /// A request to pause completed with the attached result.
    Pause(Result<(), std::io::Error>),
}

/// An event raised by a migration task that must be handled by the state
/// worker.
#[derive(Debug)]
enum MigrateTaskEvent<T> {
    /// The task completed with the associated result.
    TaskExited(Result<(), MigrateError>),

    /// The task sent a command requesting work.
    Command(T),
}

/// An event raised by some component in the instance (e.g. a vCPU or the
/// chipset) that the state worker must handle.
///
/// The vCPU-sourced events carry a time element (duration since VM boot) as
/// emitted by the kernel vmm.  This is used to deduplicate events when all
/// vCPUs running in-kernel are kicked out for the suspend state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum GuestEvent {
    /// VM entered halt state
    VcpuSuspendHalt(Duration),
    /// VM entered reboot state
    VcpuSuspendReset(Duration),
    /// vCPU encounted triple-fault
    VcpuSuspendTripleFault(i32, Duration),
    /// Chipset signaled halt condition
    ChipsetHalt,
    /// Chipset signaled reboot condition
    ChipsetReset,
}

/// Shared instance state guarded by the controller's state mutex. This state is
/// accessed from the controller API and the VM's state worker.
#[derive(Debug)]
struct SharedVmStateInner {
    external_request_queue: ExternalRequestQueue,

    /// The state worker's queue of unprocessed events from guest devices.
    guest_event_queue: VecDeque<GuestEvent>,

    /// The expected ID of the next live migration this instance will
    /// participate in (either in or out). If this is `Some`, external callers
    /// who query migration state will observe that a live migration is in
    /// progress even if the state driver has yet to pick up the live migration
    /// tasks from its queue.
    pending_migration_id: Option<Uuid>,
}

impl SharedVmStateInner {
    fn new(parent_log: &Logger) -> Self {
        let queue_log =
            parent_log.new(slog::o!("component" => "external_request_queue"));
        Self {
            external_request_queue: ExternalRequestQueue::new(queue_log),
            guest_event_queue: VecDeque::new(),
            pending_migration_id: None,
        }
    }
}

#[derive(Debug)]
pub(crate) struct SharedVmState {
    inner: Mutex<SharedVmStateInner>,
    cv: Condvar,
}

/// A VM controller: a wrapper around a Propolis instance that supplies the
/// functions needed for the Propolis server to implement its own API.
pub struct VmController {
    /// A collection of objects that don't change once an instance is ensured:
    /// the instance itself, a description of it, and convenience references to
    /// some of its members (used to avoid rummaging through the instance's
    /// inventory).
    vm_objects: VmObjects,

    /// A wrapper for the runtime state of this instance, managed by the state
    /// worker thread. This also serves as a sink for hardware events (e.g. from
    /// vCPUs and the chipset), so it is wrapped in an Arc so that it can be
    /// shared with those events' sources.
    worker_state: Arc<SharedVmState>,

    /// A handle to the state worker thread for this instance.
    worker_thread: Mutex<
        Option<JoinHandle<tokio::sync::watch::Sender<ApiMonitoredState>>>,
    >,

    /// This controller's logger.
    log: Logger,

    /// A handle to a tokio runtime onto which this controller can spawn tasks
    /// (e.g. migration tasks).
    runtime_hdl: tokio::runtime::Handle,

    /// A weak reference to this controller, suitable for upgrading and passing
    /// to tasks the controller spawns.
    this: Weak<Self>,
}

impl SharedVmState {
    fn new(parent_log: &Logger) -> Self {
        Self {
            inner: Mutex::new(SharedVmStateInner::new(parent_log)),
            cv: Condvar::new(),
        }
    }

    fn queue_external_request(
        &self,
        request: ExternalRequest,
    ) -> Result<(), RequestDeniedReason> {
        let mut inner = self.inner.lock().unwrap();
        let result = inner.external_request_queue.try_queue(request);
        if result.is_ok() {
            self.cv.notify_one();
        }
        result
    }

    fn wait_for_next_event(&self) -> StateDriverEvent {
        let guard = self.inner.lock().unwrap();
        let mut guard = self
            .cv
            .wait_while(guard, |i| {
                i.external_request_queue.is_empty()
                    && i.guest_event_queue.is_empty()
            })
            .unwrap();

        if let Some(guest_event) = guard.guest_event_queue.pop_front() {
            StateDriverEvent::Guest(guest_event)
        } else {
            StateDriverEvent::External(
                guard.external_request_queue.pop_front().unwrap(),
            )
        }
    }

    /// Add a guest event to the queue, so long as it does not appear to be a
    /// duplicate of an existing event.
    fn enqueue_guest_event(&self, event: GuestEvent) {
        let mut inner = self.inner.lock().unwrap();
        if !inner.guest_event_queue.iter().any(|ev| *ev == event) {
            // Only queue event if nothing else in the queue is a direct match
            inner.guest_event_queue.push_back(event);
            self.cv.notify_one();
        }
    }

    pub fn suspend_halt_event(&self, when: Duration) {
        self.enqueue_guest_event(GuestEvent::VcpuSuspendHalt(when));
    }

    pub fn suspend_reset_event(&self, when: Duration) {
        self.enqueue_guest_event(GuestEvent::VcpuSuspendReset(when));
    }

    pub fn suspend_triple_fault_event(&self, vcpu_id: i32, when: Duration) {
        self.enqueue_guest_event(GuestEvent::VcpuSuspendTripleFault(
            vcpu_id, when,
        ));
    }

    pub fn unhandled_vm_exit(
        &self,
        vcpu_id: i32,
        exit: propolis::exits::VmExitKind,
    ) {
        panic!("vCPU {}: Unhandled VM exit: {:?}", vcpu_id, exit);
    }

    pub fn io_error_event(&self, vcpu_id: i32, error: std::io::Error) {
        panic!("vCPU {}: Unhandled vCPU error: {}", vcpu_id, error);
    }
}

/// Functions called by a Propolis chipset to notify another component that an
/// event occurred.
pub trait ChipsetEventHandler: Send + Sync {
    fn chipset_halt(&self);
    fn chipset_reset(&self);
}

impl ChipsetEventHandler for SharedVmState {
    fn chipset_halt(&self) {
        self.enqueue_guest_event(GuestEvent::ChipsetHalt);
    }

    fn chipset_reset(&self) {
        self.enqueue_guest_event(GuestEvent::ChipsetReset);
    }
}

impl VmController {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        instance_spec: VersionedInstanceSpec,
        properties: InstanceProperties,
        &StaticConfig { vm: ref toml_config, use_reservoir, .. }: &StaticConfig,
        producer_registry: Option<ProducerRegistry>,
        nexus_client: Option<NexusClient>,
        log: Logger,
        runtime_hdl: tokio::runtime::Handle,
        stop_ch: oneshot::Sender<()>,
    ) -> anyhow::Result<Arc<Self>> {
        let bootrom = &toml_config.bootrom;
        info!(log, "initializing new VM";
              "spec" => #?instance_spec,
              "properties" => #?properties,
              "use_reservoir" => use_reservoir,
              "bootrom" => %bootrom.display());

        let vmm_log = log.new(slog::o!("component" => "vmm"));

        // Set up the 'shell' instance into which the rest of this routine will
        // add components.
        let VersionedInstanceSpec::V0(v0_spec) = &instance_spec;
        let machine = build_instance(
            &properties.id.to_string(),
            v0_spec,
            use_reservoir,
            vmm_log,
        )?;

        // Create the state monitor channel and the worker state struct that
        // depends on it. The state struct can then be passed to device
        // initialization as an event sink.
        let (monitor_tx, monitor_rx) =
            tokio::sync::watch::channel(ApiMonitoredState {
                gen: 0,
                state: ApiInstanceState::Creating,
                migration: None,
            });

        let worker_state = Arc::new(SharedVmState::new(&log));

        // Create and initialize devices in the new instance.
        let mut init = MachineInitializer {
            log: log.clone(),
            machine: &machine,
            devices: DeviceMap::new(),
            block_backends: BlockBackendMap::new(),
            crucible_backends: CrucibleBackendMap::new(),
            spec: v0_spec,
            producer_registry,
        };

        init.initialize_rom(bootrom.as_path())?;
        let chipset = init.initialize_chipset(
            &(worker_state.clone() as Arc<dyn ChipsetEventHandler>),
        )?;
        init.initialize_rtc(&chipset)?;
        init.initialize_hpet()?;

        let com1 = Arc::new(init.initialize_uart(&chipset)?);
        let ps2ctrl = init.initialize_ps2(&chipset)?;
        init.initialize_qemu_debug_port()?;
        init.initialize_qemu_pvpanic(properties.id)?;
        init.initialize_network_devices(&chipset)?;

        #[cfg(not(feature = "omicron-build"))]
        init.initialize_test_devices(&toml_config.devices)?;
        #[cfg(feature = "omicron-build")]
        info!(
            log,
            "`omicron-build` feature enabled, ignoring any test devices"
        );

        #[cfg(feature = "falcon")]
        init.initialize_softnpu_ports(&chipset)?;
        #[cfg(feature = "falcon")]
        init.initialize_9pfs(&chipset)?;
        init.initialize_storage_devices(&chipset, nexus_client)?;
        let ramfb = init.initialize_fwcfg(v0_spec.devices.board.cpus)?;
        init.initialize_cpus()?;
        let vcpu_tasks = super::vcpu_tasks::VcpuTasks::new(
            &machine,
            worker_state.clone(),
            log.new(slog::o!("component" => "vcpu_tasks")),
        )?;

        let MachineInitializer {
            devices,
            block_backends,
            crucible_backends,
            ..
        } = init;

        // The instance is fully set up; pass it to the new controller.
        let shared_state_for_worker = worker_state.clone();
        let controller = Arc::new_cyclic(|this| Self {
            vm_objects: VmObjects {
                machine: Some(machine),
                properties,
                spec: tokio::sync::Mutex::new(instance_spec),
                devices,
                block_backends,
                crucible_backends,
                com1,
                framebuffer: Some(ramfb),
                ps2ctrl,
                monitor_rx,
            },
            worker_state,
            worker_thread: Mutex::new(None),
            log: log.new(slog::o!("component" => "vm_controller")),
            runtime_hdl: runtime_hdl.clone(),
            this: this.clone(),
        });

        // Now that the controller exists, launch the state worker that will
        // drive state transitions for this instance. When the VM halts, the
        // worker will exit and drop its reference to the controller.
        let ctrl_for_worker = controller.clone();
        let log_for_worker =
            log.new(slog::o!("component" => "vm_state_worker"));
        let worker_thread = std::thread::Builder::new()
            .name("vm_state_worker".to_string())
            .spawn(move || {
                let driver = state_driver::StateDriver::new(
                    runtime_hdl,
                    ctrl_for_worker,
                    shared_state_for_worker,
                    vcpu_tasks,
                    log_for_worker,
                    monitor_tx,
                );

                let monitor_tx = driver.run_state_worker();

                // Signal back to the server state once the worker has exited.
                let _ = stop_ch.send(());
                monitor_tx
            })
            .map_err(VmControllerError::StateWorkerCreationFailed)?;

        *controller.worker_thread.lock().unwrap() = Some(worker_thread);
        Ok(controller)
    }

    pub fn properties(&self) -> &InstanceProperties {
        &self.vm_objects.properties
    }

    pub fn machine(&self) -> &Machine {
        // Unwrap safety: The machine is created when the controller is created
        // and removed only when the controller is dropped.
        self.vm_objects
            .machine
            .as_ref()
            .expect("VM controller always has a valid machine")
    }

    pub async fn instance_spec(
        &self,
    ) -> tokio::sync::MutexGuard<'_, VersionedInstanceSpec> {
        self.vm_objects.spec.lock().await
    }

    pub fn com1(&self) -> &Arc<Serial<LpcUart>> {
        &self.vm_objects.com1
    }

    pub fn framebuffer(&self) -> Option<&Arc<RamFb>> {
        self.vm_objects.framebuffer.as_ref()
    }

    pub fn ps2ctrl(&self) -> &Arc<PS2Ctrl> {
        &self.vm_objects.ps2ctrl
    }

    pub fn crucible_backends(
        &self,
    ) -> &BTreeMap<Uuid, Arc<propolis::block::CrucibleBackend>> {
        &self.vm_objects.crucible_backends
    }

    pub fn log(&self) -> &Logger {
        &self.log
    }

    pub fn external_instance_state(&self) -> ApiInstanceState {
        self.vm_objects.monitor_rx.borrow().state
    }

    pub fn inject_nmi(&self) {
        if let Some(machine) = &self.vm_objects.machine {
            match machine.inject_nmi() {
                Ok(_) => {
                    info!(self.log, "Sending NMI to instance");
                }
                Err(e) => {
                    error!(self.log, "Could not send NMI to instance: {}", e);
                }
            };
        }
    }

    pub fn state_watcher(
        &self,
    ) -> &tokio::sync::watch::Receiver<ApiMonitoredState> {
        &self.vm_objects.monitor_rx
    }

    /// Asks to queue a request to start a source migration task for this VM.
    /// The migration will have the supplied `migration_id` and will obtain its
    /// connection to the target by calling `upgrade_fn` to obtain a future that
    /// yields the necessary connection.
    ///
    /// This routine fails if the VM was not marked as a migration source or if
    /// it has another pending request that precludes migration. Note that this
    /// routine does not fail if the future returned from `upgrade_fn` fails to
    /// produce a connection to the destination.
    ///
    /// On success, clients may query the instance's migration status to
    /// determine how the migration has progressed.
    pub fn request_migration_from<
        T: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    >(
        &self,
        migration_id: Uuid,
        conn: WebSocketStream<T>,
        protocol: crate::migrate::protocol::Protocol,
    ) -> Result<(), VmControllerError> {
        let mut inner = self.worker_state.inner.lock().unwrap();

        // Check that the request can be enqueued before setting up the
        // migration task.
        if !inner.external_request_queue.migrate_as_source_will_enqueue()? {
            return Ok(());
        }

        let migration_request =
            self.launch_source_migration_task(migration_id, conn, protocol);

        // Unwrap is safe because the queue state was checked under the lock.
        inner.external_request_queue.try_queue(migration_request).unwrap();
        self.worker_state.cv.notify_one();
        Ok(())
    }

    /// Launches a task that will execute a live migration out of this VM.
    /// Returns a state change request message to queue to the state driver,
    /// which will coordinate with this task to run the migration.
    fn launch_source_migration_task<
        T: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    >(
        &self,
        migration_id: Uuid,
        conn: WebSocketStream<T>,
        protocol: crate::migrate::protocol::Protocol,
    ) -> ExternalRequest {
        let log_for_task =
            self.log.new(slog::o!("component" => "migrate_source_task"));
        let ctrl_for_task = self.this.upgrade().unwrap();
        let (start_tx, start_rx) = tokio::sync::oneshot::channel();
        let (command_tx, command_rx) = tokio::sync::mpsc::channel(1);
        let (response_tx, response_rx) = tokio::sync::mpsc::channel(1);

        // The migration process uses async operations when communicating with
        // the migration target. Run that work on the async runtime.
        info!(self.log, "Launching migration source task");
        let task = self.runtime_hdl.spawn(async move {
            info!(log_for_task, "Waiting to be told to start");
            start_rx.await.unwrap();

            info!(log_for_task, "Starting migration procedure");
            if let Err(e) = crate::migrate::source::migrate(
                ctrl_for_task,
                command_tx,
                response_rx,
                conn,
                protocol,
            )
            .await
            {
                error!(log_for_task, "Migration task failed: {}", e);
                return Err(e);
            }

            Ok(())
        });

        ExternalRequest::MigrateAsSource {
            migration_id,
            task,
            start_tx,
            command_rx,
            response_tx,
        }
    }

    /// Asks to queue a request to start a destination migration task for this
    /// VM. The migration will have the supplied `migration_id` and will obtain
    /// its connection to the source by calling `upgrade_fn` to obtain a future
    /// that yields the necessary connection.
    ///
    /// This routine fails if the VM has already begun to run or if a previous
    /// migration in was attempted (regardless of its outcome). Note that this
    /// routine does not fail if the future returned from `upgrade_fn`
    /// subsequently fails to produce a connection to the destination (though
    /// the migration attempt will then fail).
    ///
    /// On success, clients may query the instance's migration status to
    /// determine how the migration has progressed.
    pub fn request_migration_into<
        T: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    >(
        &self,
        migration_id: Uuid,
        conn: WebSocketStream<T>,
        local_addr: SocketAddr,
        protocol: crate::migrate::protocol::Protocol,
    ) -> Result<(), VmControllerError> {
        let mut inner = self.worker_state.inner.lock().unwrap();
        if !inner.external_request_queue.migrate_as_target_will_enqueue()? {
            return Ok(());
        }

        // Check that the request can be enqueued before setting up the
        // migration task.
        let migration_request = self.launch_target_migration_task(
            migration_id,
            conn,
            local_addr,
            protocol,
        );

        // Unwrap is safe because the queue state was checked under the lock.
        inner.external_request_queue.try_queue(migration_request).unwrap();
        self.worker_state.cv.notify_one();
        Ok(())
    }

    /// Launches a task that will execute a live migration into this VM.
    /// Returns a state change request message to queue to the state driver,
    /// which will coordinate with this task to run the migration.
    fn launch_target_migration_task<
        T: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    >(
        &self,
        migration_id: Uuid,
        conn: WebSocketStream<T>,
        local_addr: SocketAddr,
        protocol: crate::migrate::protocol::Protocol,
    ) -> ExternalRequest {
        let log_for_task =
            self.log.new(slog::o!("component" => "migrate_source_task"));
        let ctrl_for_task = self.this.upgrade().unwrap();
        let (start_tx, start_rx) = tokio::sync::oneshot::channel();
        let (command_tx, command_rx) = tokio::sync::mpsc::channel(1);

        // The migration process uses async operations when communicating with
        // the migration target. Run that work on the async runtime.
        info!(self.log, "Launching migration target task");
        let task = self.runtime_hdl.spawn(async move {
            info!(log_for_task, "Waiting to be told to start");
            start_rx.await.unwrap();

            info!(log_for_task, "Starting migration procedure");
            if let Err(e) = crate::migrate::destination::migrate(
                ctrl_for_task,
                command_tx,
                conn,
                local_addr,
                protocol,
            )
            .await
            {
                error!(log_for_task, "Migration task failed: {}", e);
                return Err(e);
            }

            Ok(())
        });

        ExternalRequest::MigrateAsTarget {
            migration_id,
            task,
            start_tx,
            command_rx,
        }
    }

    /// Handles a request to change the wrapped instance's state.
    pub fn put_state(
        &self,
        requested: ApiInstanceStateRequested,
    ) -> Result<(), VmControllerError> {
        info!(self.log(), "Requested state {:?} via API", requested);

        self.worker_state
            .queue_external_request(match requested {
                ApiInstanceStateRequested::Run => ExternalRequest::Start,
                ApiInstanceStateRequested::Stop => ExternalRequest::Stop,
                ApiInstanceStateRequested::Reboot => ExternalRequest::Reboot,
            })
            .map_err(Into::into)
    }

    pub fn migrate_status(
        &self,
        migration_id: Uuid,
    ) -> Result<ApiMigrationState, MigrateError> {
        // If the state worker has published migration state with a matching ID,
        // report the status from the worker. Note that this call to `borrow`
        // takes a lock on the channel.
        let published = self.vm_objects.monitor_rx.borrow();
        if let Some(status) = &published.migration {
            if status.migration_id == migration_id {
                return Ok(status.state);
            }
        }
        drop(published);

        // Either the worker hasn't published any status or the IDs didn't
        // match. See if there's a pending migration task that the worker hasn't
        // picked up yet that has the correct ID and report its status if so.
        let inner = self.worker_state.inner.lock().unwrap();
        if let Some(id) = inner.pending_migration_id {
            if migration_id != id {
                Err(MigrateError::UuidMismatch)
            } else {
                Ok(ApiMigrationState::Sync)
            }
        } else {
            Err(MigrateError::NoMigrationInProgress)
        }
    }

    pub(crate) fn for_each_device(
        &self,
        mut func: impl FnMut(&str, &Arc<dyn propolis::common::Lifecycle>),
    ) {
        for (name, dev) in self.vm_objects.devices.iter() {
            func(name, dev);
        }
    }

    pub(crate) fn for_each_device_fallible<F, E>(
        &self,
        mut func: F,
    ) -> std::result::Result<(), E>
    where
        F: FnMut(
            &str,
            &Arc<dyn propolis::common::Lifecycle>,
        ) -> std::result::Result<(), E>,
    {
        for (name, dev) in self.vm_objects.devices.iter() {
            func(name, dev)?;
        }
        Ok(())
    }

    pub(crate) fn device_by_name(
        &self,
        name: &String,
    ) -> Option<Arc<dyn propolis::common::Lifecycle>> {
        self.vm_objects.devices.get(name).map(Arc::clone)
    }
}

impl Drop for VmController {
    fn drop(&mut self) {
        info!(self.log, "Dropping VM controller");
        let instance = self
            .vm_objects
            .machine
            .take()
            .expect("VM controller should have an instance at drop");
        drop(instance);

        // A fully-initialized controller is kept alive in part by its worker
        // thread, which owns the sender side of the controller's state-change
        // notification channel. Since the controller is being dropped, the
        // worker is gone, so reclaim the sender from it and use it to publish
        // that the controller is being destroyed.
        if let Some(thread) = self.worker_thread.lock().unwrap().take() {
            let api_state = thread.join().unwrap();
            let old_state = api_state.borrow().clone();

            // Preserve the instance's state if it failed so that clients can
            // distinguish gracefully-stopped instances from failed instances.
            if matches!(old_state.state, ApiInstanceState::Failed) {
                return;
            }

            let gen = old_state.gen + 1;
            let _ = api_state.send(ApiMonitoredState {
                gen,
                state: ApiInstanceState::Destroyed,
                ..old_state
            });
        }
    }
}

/// An event that a VM's state driver must process.
#[derive(Debug)]
enum StateDriverEvent {
    /// An event that was raised from within the guest.
    Guest(GuestEvent),

    /// An event that was raised by an external entity (e.g. an API call to the
    /// server).
    External(ExternalRequest),
}

/// Commands issued by the state driver back to its VM controller. These are
/// abstracted into a trait to allow them to be mocked out for testing without
/// having to supply mock implementations of the rest of the VM controller's
/// functionality.
#[cfg_attr(test, mockall::automock)]
trait StateDriverVmController {
    /// Pause VM at the kernel VMM level, ensuring that in-kernel-emulated
    /// devices and vCPUs are brought to a consistent state.
    ///
    /// When the VM is paused, attempts to run its vCPUs (via `VM_RUN` ioctl)
    /// will fail.  A corresponding `resume_vm()` call must be made prior to
    /// allowing vCPU tasks to run.
    fn pause_vm(&self);

    /// Resume a previously-paused VM at the kernel VMM level.  This will resume
    /// any timers driving in-kernel-emulated devices, and allow the vCPU to run
    /// again.
    fn resume_vm(&self);

    /// Sends a reset request to each device in the instance, then sends a
    /// reset command to the instance's bhyve VM.
    fn reset_devices_and_machine(&self);

    /// Sends each device (and backend) a start request.
    fn start_devices(&self) -> anyhow::Result<()>;

    /// Sends each device a pause request, then waits for all these requests to
    /// complete.
    fn pause_devices(&self);

    /// Sends each device a resume request.
    fn resume_devices(&self);

    /// Sends each device (and backend) a halt request.
    fn halt_devices(&self);

    /// Resets the state of each vCPU in the instance to its on-reboot state.
    fn reset_vcpu_state(&self);
}

impl StateDriverVmController for VmController {
    fn pause_vm(&self) {
        info!(self.log, "Pausing kernel VMM resources");
        self.machine().hdl.pause().expect("VM_PAUSE should succeed")
    }

    fn resume_vm(&self) {
        info!(self.log, "Resuming kernel VMM resources");
        self.machine().hdl.resume().expect("VM_RESUME should succeed")
    }

    fn reset_devices_and_machine(&self) {
        let _rtguard = self.runtime_hdl.enter();
        self.for_each_device(|name, dev| {
            info!(self.log, "Sending reset request to {}", name);
            dev.reset();
        });

        self.machine().reinitialize().unwrap();
    }

    fn start_devices(&self) -> anyhow::Result<()> {
        let _rtguard = self.runtime_hdl.enter();
        self.for_each_device_fallible(|name, dev| {
            info!(self.log, "Sending startup complete to {}", name);
            let res = dev.start();
            if let Err(e) = &res {
                error!(self.log, "Startup failed for {}: {:?}", name, e);
            }
            res
        })?;
        for (name, backend) in self.vm_objects.block_backends.iter() {
            debug!(self.log, "Starting block backend {}", name);
            let res = backend.start();
            if let Err(e) = &res {
                error!(self.log, "Startup failed for {}: {:?}", name, e);
                return res;
            }
        }
        Ok(())
    }

    fn pause_devices(&self) {
        let _rtguard = self.runtime_hdl.enter();
        self.for_each_device(|name, dev| {
            info!(self.log, "Sending pause request to {}", name);
            dev.pause();
        });

        // Create a Future that returns the name of the device that has finished
        // pausing: this allows keeping track of which devices have and haven't
        // completed pausing yet.
        struct NamedFuture {
            name: String,
            future: BoxFuture<'static, ()>,
        }

        impl std::future::Future for NamedFuture {
            type Output = String;

            fn poll(
                self: Pin<&mut Self>,
                cx: &mut Context<'_>,
            ) -> Poll<Self::Output> {
                let mut_self = self.get_mut();
                match Pin::new(&mut mut_self.future).poll(cx) {
                    Poll::Pending => Poll::Pending,
                    Poll::Ready(()) => Poll::Ready(mut_self.name.clone()),
                }
            }
        }

        info!(self.log, "Waiting for devices to pause");
        self.runtime_hdl.block_on(async {
            let mut stream: FuturesUnordered<_> = self
                .vm_objects
                .devices
                .iter()
                .map(|(name, dev)| {
                    info!(self.log, "Got paused future from dev {}", name);
                    NamedFuture { name: name.to_string(), future: dev.paused() }
                })
                .collect();

            loop {
                match stream.next().await {
                    Some(name) => {
                        info!(self.log, "dev {} completed pause", name);
                    }

                    None => {
                        // done
                        info!(self.log, "all devices paused");
                        break;
                    }
                }
            }
        });
    }

    fn resume_devices(&self) {
        let _rtguard = self.runtime_hdl.enter();
        self.for_each_device(|name, dev| {
            info!(self.log, "Sending resume request to {}", name);
            dev.resume();
        });
    }

    fn halt_devices(&self) {
        let _rtguard = self.runtime_hdl.enter();
        self.for_each_device(|name, dev| {
            info!(self.log, "Sending halt request to {}", name);
            dev.halt();
        });
        for (name, backend) in self.vm_objects.block_backends.iter() {
            debug!(self.log, "Halting block backend {}", name);
            backend.halt();
        }
    }

    fn reset_vcpu_state(&self) {
        for vcpu in self.machine().vcpus.iter() {
            info!(self.log, "Resetting vCPU {}", vcpu.id);
            vcpu.activate().unwrap();
            vcpu.reboot_state().unwrap();
            if vcpu.is_bsp() {
                info!(self.log, "Resetting BSP vCPU {}", vcpu.id);
                vcpu.set_run_state(propolis::bhyve_api::VRS_RUN, None).unwrap();
                vcpu.set_reg(
                    propolis::bhyve_api::vm_reg_name::VM_REG_GUEST_RIP,
                    0xfff0,
                )
                .unwrap();
            }
        }
    }
}
