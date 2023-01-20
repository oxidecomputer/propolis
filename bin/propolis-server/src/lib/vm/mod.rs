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

use std::{
    collections::{BTreeMap, VecDeque},
    fmt::Debug,
    path::PathBuf,
    sync::{Arc, Condvar, Mutex, Weak},
    thread::JoinHandle,
};

use oximeter::types::ProducerRegistry;
use propolis::{
    hw::{ps2::ctrl::PS2Ctrl, qemu::ramfb::RamFb, uart::LpcUart},
    Instance,
};
use propolis_client::handmade::{
    api::InstanceProperties, api::InstanceState as ApiInstanceState,
    api::InstanceStateMonitorResponse as ApiMonitoredState,
    api::InstanceStateRequested as ApiInstanceStateRequested,
    api::MigrationState as ApiMigrationState,
};
use propolis_client::instance_spec::InstanceSpec;
use slog::{error, info, Logger};
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::oneshot;
use tokio_tungstenite::WebSocketStream;
use uuid::Uuid;

use crate::{
    initializer::{build_instance, MachineInitializer},
    migrate::MigrateError,
    serial::Serial,
};

mod state_driver;

#[derive(Debug, Error)]
pub enum VmControllerError {
    #[error("The requested operation requires an active instance")]
    InstanceNotActive,

    #[error("The instance has a pending request to halt")]
    InstanceHaltPending,

    #[error("Instance is not marked as a possible migration source")]
    NotMarkedMigrationSource,

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

    #[error("Cannot request state {0:?} while in lifecycle stage {1:?}")]
    InvalidStageForRequest(ApiInstanceStateRequested, LifecycleStage),

    #[error(
        "Cannot ask to be a migration target while in lifecycle stage \
            {0:?}"
    )]
    InvalidStageForMigrationTarget(LifecycleStage),

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
            | VmControllerError::NotMarkedMigrationSource
            | VmControllerError::InvalidRequestForMigrationSource(_)
            | VmControllerError::MigrationTargetInProgress
            | VmControllerError::MigrationTargetFailed
            | VmControllerError::TooLateToBeMigrationTarget
            | VmControllerError::InvalidStageForRequest(_, _)
            | VmControllerError::InvalidStageForMigrationTarget(_)
            | VmControllerError::InstanceNotActive
            | VmControllerError::InstanceHaltPending
            | VmControllerError::MigrationTargetPreviouslyCompleted => {
                HttpError::for_bad_request(
                    None,
                    format!("Instance operation failed: {}", vm_error),
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
    /// The underlying Propolis `Instance` this controller is managing.
    instance: Option<Instance>,

    /// The instance properties supplied when this controller was created.
    properties: InstanceProperties,

    /// The instance spec used to create this controller's VM.
    spec: InstanceSpec,

    /// A wrapper around the instance's first COM port, suitable for providing a
    /// connection to a guest's serial console.
    com1: Arc<Serial<LpcUart>>,

    /// An optional reference to the guest's virtual framebuffer.
    framebuffer: Option<Arc<RamFb>>,

    /// An optional reference to the guest's virtual ps2 controller.
    ps2ctrl: Option<Arc<PS2Ctrl>>,

    /// A map of the instance's active Crucible backends.
    crucible_backends: BTreeMap<Uuid, Arc<propolis::block::CrucibleBackend>>,

    /// A notification receiver to which the state worker publishes the most
    /// recent instance state and state generation.
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

    /// Pause the instance's entities and CPUs.
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

/// The subordinate state of an instance that has not yet been asked to run.
/// Dictates whether the instance can start and, if so, what startup procedure
/// should be used.
#[derive(Clone, Copy, Debug)]
pub enum StartupStage {
    /// The instance's entities should be started from their initial, cold-reset
    /// state.
    ColdBoot,

    /// The instance cannot run yet because a client asked it to serve as a
    /// migration target, and that migration is not finished yet.
    MigratePending,

    /// The instance successfully initialized via live migration.
    Migrated,

    /// The instance tried to initialize via live migration, but the migration
    /// failed.
    MigrationFailed,
}

/// A logical stage in a VM's lifecycle.
#[derive(Clone, Copy, Debug)]
pub enum LifecycleStage {
    /// The VM has not started yet and is in the specified phase of its
    /// pre-start lifecycle.
    NotStarted(StartupStage),

    /// The VM has been launched. Note that it may be paused.
    Active,

    /// The VM has been stopped and will not restart.
    NoLongerActive,
}

/// An external request made of the controller via the server API. Handled by
/// the controller's worker thread.
#[derive(Debug)]
enum ExternalRequest {
    /// Initializes the VM through live migration by running a
    /// migration-destination task.
    MigrateAsTarget {
        /// The ID of the live migration to use when initializing.
        migration_id: Uuid,

        /// A handle to the task that will execute the migration procedure.
        task: tokio::task::JoinHandle<Result<(), MigrateError>>,

        /// The sender side of a one-shot channel that, when signaled, tells the
        /// migration task to start its work.
        start_tx: tokio::sync::oneshot::Sender<()>,

        /// A channel that receives commands from the migration task.
        command_rx: tokio::sync::mpsc::Receiver<MigrateTargetCommand>,
    },

    /// Starts the VM.
    Start {
        /// Indicates whether the VM's entities and CPUs should be fully reset
        /// before starting.
        reset_required: bool,
    },

    /// Asks the state worker to start a migration-source task.
    MigrateAsSource {
        /// The ID of the live migration for which this VM will be the source.
        migration_id: Uuid,

        /// A handle to the task that will execute the migration procedure.
        task: tokio::task::JoinHandle<Result<(), MigrateError>>,

        /// The sender side of a one-shot channel that, when signaled, tells the
        /// migration task to start its work.
        start_tx: tokio::sync::oneshot::Sender<()>,

        /// A channel that receives commands from the migration task.
        command_rx: tokio::sync::mpsc::Receiver<MigrateSourceCommand>,

        /// A channel used to send responses to migration commands.
        response_tx: tokio::sync::mpsc::Sender<MigrateSourceResponse>,
    },

    /// Resets the guest by pausing all devices, resetting them to their
    /// cold-boot states, and resuming the devices. Note that this is not a
    /// graceful reboot and does not coordinate with guest software.
    Reboot,

    /// Halts the VM. Note that this is not a graceful shutdown and does not
    /// coordinate with guest software.
    Stop,
}

/// An event raised by some component in the instance (e.g. a vCPU or the
/// chipset) that the state worker must handle.
#[derive(Clone, Copy, Debug)]
enum GuestEvent {
    VcpuSuspendHalt(i32),
    VcpuSuspendReset(i32),
    VcpuSuspendTripleFault(i32),
    ChipsetHalt,
    ChipsetReset,
}

/// Shared instance state guarded by the controller's state mutex. This state is
/// accessed from the controller API and the VM's state worker.
#[derive(Debug)]
struct SharedVmStateInner {
    /// The VM's lifecycle stage. This can be used to determine whether incoming
    /// API requests are allowed--for example, requesting to migrate into a VM
    /// that's already running is forbidden.
    lifecycle_stage: LifecycleStage,

    /// True if the instance previously received an API call denoting that it
    /// should expect a request to act as a live migration source. If false,
    /// incoming requests to migrate from a prospective destination should be
    /// rejected.
    marked_migration_source: bool,

    /// True if the instance's request queue has a pending request to migrate to
    /// another instance. If true, external API requests that may need to be
    /// serviced by the target should be blocked until this is cleared. (Clients
    /// can monitor the state of the migration and retry once it has resolved.)
    migrate_from_pending: bool,

    /// True if the instance's request queue has a pending request to stop the
    /// instance. If true, external API requests that only make sense on a
    /// running instance should be blocked.
    halt_pending: bool,

    /// The state worker's queue of pending requests from the server API.
    external_request_queue: VecDeque<ExternalRequest>,

    /// The state worker's queue of unprocessed events from guest devices.
    guest_event_queue: VecDeque<GuestEvent>,

    /// The sender side of the watcher that records the instance's externally
    /// visible state, i.e., the state returned when a client invokes the
    /// server's `get` API.
    api_state: tokio::sync::watch::Sender<ApiMonitoredState>,

    /// The state of the most recently attempted migration into or out of this
    /// instance, or None if no migration has ever been attempted.
    migration_state: Option<(Uuid, ApiMigrationState)>,
}

impl SharedVmStateInner {
    fn new(watch: tokio::sync::watch::Sender<ApiMonitoredState>) -> Self {
        Self {
            lifecycle_stage: LifecycleStage::NotStarted(StartupStage::ColdBoot),
            marked_migration_source: false,
            migrate_from_pending: false,
            halt_pending: false,
            external_request_queue: VecDeque::new(),
            guest_event_queue: VecDeque::new(),
            api_state: watch,
            migration_state: None,
        }
    }

    /// Updates an instance's externally visible state by incrementing the
    /// supplied generation number and writing the incremented value and the
    /// supplied state to the worker's watcher.
    fn update_external_state(
        &mut self,
        gen: &mut u64,
        state: ApiInstanceState,
    ) {
        *gen += 1;

        // Unwrap is safe here because this routine is only called from the
        // state worker, and if the state worker is running, the VM controller
        // must be alive, which implies that its embedded receiver is alive.
        self.api_state.send(ApiMonitoredState { gen: *gen, state }).unwrap();
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
    worker_thread: Mutex<Option<JoinHandle<()>>>,

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
    pub(crate) fn suspend_halt_event(&self, vcpu_id: i32) {
        let mut inner = self.inner.lock().unwrap();
        inner.guest_event_queue.push_back(GuestEvent::VcpuSuspendHalt(vcpu_id));
        self.cv.notify_one();
    }

    pub(crate) fn suspend_reset_event(&self, vcpu_id: i32) {
        let mut inner = self.inner.lock().unwrap();
        inner
            .guest_event_queue
            .push_back(GuestEvent::VcpuSuspendReset(vcpu_id));
        self.cv.notify_one();
    }

    pub(crate) fn suspend_triple_fault_event(&self, vcpu_id: i32) {
        let mut inner = self.inner.lock().unwrap();
        inner
            .guest_event_queue
            .push_back(GuestEvent::VcpuSuspendTripleFault(vcpu_id));
        self.cv.notify_one();
    }

    pub(crate) fn unhandled_vm_exit(
        &self,
        vcpu_id: i32,
        exit: propolis::exits::VmExitKind,
    ) {
        panic!("vCPU {}: Unhandled VM exit: {:?}", vcpu_id, exit);
    }

    pub(crate) fn io_error_event(&self, vcpu_id: i32, error: std::io::Error) {
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
        let mut inner = self.inner.lock().unwrap();
        inner.guest_event_queue.push_back(GuestEvent::ChipsetHalt);
        self.cv.notify_one();
    }

    fn chipset_reset(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.guest_event_queue.push_back(GuestEvent::ChipsetReset);
        self.cv.notify_one();
    }
}

impl VmController {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        instance_spec: InstanceSpec,
        properties: InstanceProperties,
        use_reservoir: bool,
        bootrom: PathBuf,
        oximeter_registry: Option<ProducerRegistry>,
        log: Logger,
        runtime_hdl: tokio::runtime::Handle,
        stop_ch: oneshot::Sender<()>,
    ) -> anyhow::Result<Arc<Self>> {
        let vmm_log = log.new(slog::o!("component" => "vmm"));

        // Set up the 'shell' instance into which the rest of this routine will
        // add components.
        let instance = build_instance(
            &properties.id.to_string(),
            &instance_spec,
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
            });
        let worker_state = Arc::new(SharedVmState {
            inner: Mutex::new(SharedVmStateInner::new(monitor_tx)),
            cv: Condvar::new(),
        });

        // Create and initialize devices in the new instance.
        let instance_inner = instance.lock();
        let inv = instance_inner.inventory();
        let machine = instance_inner.machine();
        let init = MachineInitializer::new(
            log.clone(),
            machine,
            inv,
            &instance_spec,
            oximeter_registry,
        );

        init.initialize_rom(bootrom)?;
        init.initialize_kernel_devs()?;
        let chipset = init.initialize_chipset(
            &(worker_state.clone() as Arc<dyn ChipsetEventHandler>),
        )?;

        let com1 = Arc::new(init.initialize_uart(&chipset)?);
        let ps2ctrl_id = init.initialize_ps2(&chipset)?;
        let ps2ctrl: Option<Arc<PS2Ctrl>> = inv.get_concrete(ps2ctrl_id);
        init.initialize_qemu_debug_port()?;
        init.initialize_network_devices(&chipset)?;
        #[cfg(feature = "falcon")]
        init.initialize_softnpu_ports(&chipset)?;
        #[cfg(feature = "falcon")]
        init.initialize_9pfs(&chipset)?;
        let crucible_backends = init.initialize_storage_devices(&chipset)?;
        let framebuffer_id =
            init.initialize_fwcfg(instance_spec.devices.board.cpus)?;
        let framebuffer: Option<Arc<RamFb>> = inv.get_concrete(framebuffer_id);
        init.initialize_cpus()?;
        let vcpu_tasks = super::vcpu_tasks::VcpuTasks::new(
            instance_inner,
            worker_state.clone(),
            log.new(slog::o!("component" => "vcpu_tasks")),
        )?;

        // The instance is fully set up; pass it to the new controller.
        let controller = Arc::new_cyclic(|this| Self {
            vm_objects: VmObjects {
                instance: Some(instance),
                properties,
                spec: instance_spec,
                com1,
                framebuffer,
                ps2ctrl,
                crucible_backends,
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
                let state_gen =
                    ctrl_for_worker.vm_objects.monitor_rx.borrow().gen;
                let mut driver = state_driver::StateDriver::new(
                    runtime_hdl,
                    ctrl_for_worker,
                    vcpu_tasks,
                    log_for_worker,
                    state_gen,
                );
                driver.run_state_worker();

                // Signal back to the server state once the worker has exited.
                let _ = stop_ch.send(());
            })
            .map_err(VmControllerError::StateWorkerCreationFailed)?;

        *controller.worker_thread.lock().unwrap() = Some(worker_thread);
        Ok(controller)
    }

    pub fn properties(&self) -> &InstanceProperties {
        &self.vm_objects.properties
    }

    pub fn instance(&self) -> &Instance {
        // Unwrap safety: The instance is created when the controller is created
        // and removed only when the controller is dropped.
        self.vm_objects
            .instance
            .as_ref()
            .expect("VM controller always has a valid instance")
    }

    pub fn instance_spec(&self) -> &InstanceSpec {
        &self.vm_objects.spec
    }

    pub fn com1(&self) -> &Arc<Serial<LpcUart>> {
        &self.vm_objects.com1
    }

    pub fn framebuffer(&self) -> Option<&Arc<RamFb>> {
        self.vm_objects.framebuffer.as_ref()
    }

    pub fn ps2ctrl(&self) -> Option<&Arc<PS2Ctrl>> {
        self.vm_objects.ps2ctrl.as_ref()
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
        if let Some(instance) = &self.vm_objects.instance {
            let instance = instance.lock();
            match instance.machine().inject_nmi() {
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
    ) -> Result<(), VmControllerError> {
        let mut inner = self.worker_state.inner.lock().unwrap();
        match inner.lifecycle_stage {
            LifecycleStage::NotStarted(_) => {
                assert!(!inner.marked_migration_source);
                Err(VmControllerError::InstanceNotActive)
            }
            LifecycleStage::Active => {
                if !inner.marked_migration_source {
                    Err(VmControllerError::NotMarkedMigrationSource)
                } else if inner.migrate_from_pending {
                    Err(VmControllerError::AlreadyMigrationSource)
                } else if inner.halt_pending {
                    Err(VmControllerError::InstanceHaltPending)
                } else {
                    inner.migrate_from_pending = true;

                    // Update the migration state before the task starts so that
                    // requests to query the migration state will succeed. This
                    // ensures that if a later request to change the instance's
                    // state fails because a migration is in progress, there
                    // will in fact appear to be a pending migration even if the
                    // state worker hasn't yet picked up the work.
                    inner.migration_state =
                        Some((migration_id, ApiMigrationState::Sync));
                    inner.external_request_queue.push_back(
                        self.launch_source_migration_task(migration_id, conn),
                    );
                    self.worker_state.cv.notify_one();
                    Ok(())
                }
            }
            LifecycleStage::NoLongerActive => {
                Err(VmControllerError::InstanceNotActive)
            }
        }
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
    ) -> Result<(), VmControllerError> {
        let mut inner = self.worker_state.inner.lock().unwrap();
        match inner.lifecycle_stage {
            LifecycleStage::NotStarted(substage) => match substage {
                StartupStage::ColdBoot => {
                    inner.lifecycle_stage = LifecycleStage::NotStarted(
                        StartupStage::MigratePending,
                    );

                    // Update the migration state before the task starts so that
                    // requests to query the migration state will succeed. This
                    // ensures that if a later request to change the instance's
                    // state fails because a migration is in progress, there
                    // will in fact appear to be a pending migration even if the
                    // state worker hasn't yet picked up the work.
                    inner.migration_state =
                        Some((migration_id, ApiMigrationState::Sync));
                    inner.external_request_queue.push_back(
                        self.launch_target_migration_task(migration_id, conn),
                    );
                    self.worker_state.cv.notify_one();
                    Ok(())
                }
                StartupStage::MigratePending => {
                    Err(VmControllerError::MigrationTargetInProgress)
                }
                StartupStage::Migrated => {
                    Err(VmControllerError::MigrationTargetPreviouslyCompleted)
                }
                StartupStage::MigrationFailed => {
                    Err(VmControllerError::MigrationTargetFailed)
                }
            },
            LifecycleStage::Active => {
                Err(VmControllerError::TooLateToBeMigrationTarget)
            }
            LifecycleStage::NoLongerActive => {
                Err(VmControllerError::InstanceNotActive)
            }
        }
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
        let mut inner = self.worker_state.inner.lock().unwrap();
        info!(
            self.log(),
            "Requested state {:?} via API, current worker state: {:?}",
            requested,
            inner
        );

        match requested {
            // Requests to run succeed if the VM hasn't started yet, but can be
            // started, or if it has started already.
            ApiInstanceStateRequested::Run => match inner.lifecycle_stage {
                LifecycleStage::NotStarted(substage) => {
                    let reset_required = match substage {
                        StartupStage::ColdBoot => Ok(true),
                        StartupStage::MigratePending => {
                            Err(VmControllerError::MigrationTargetInProgress)
                        }
                        StartupStage::Migrated => Ok(false),
                        StartupStage::MigrationFailed => {
                            Err(VmControllerError::MigrationTargetFailed)
                        }
                    }?;

                    inner
                        .external_request_queue
                        .push_back(ExternalRequest::Start { reset_required });
                    self.worker_state.cv.notify_one();
                    Ok(())
                }
                LifecycleStage::Active => Ok(()),
                LifecycleStage::NoLongerActive => {
                    Err(VmControllerError::InvalidStageForRequest(
                        requested,
                        inner.lifecycle_stage,
                    ))
                }
            },

            // Requests to stop always succeed. Note that a request to stop a VM
            // that hasn't started should still be queued to the state worker so
            // that the worker can exit and drop its references to the instance.
            ApiInstanceStateRequested::Stop => match inner.lifecycle_stage {
                LifecycleStage::NotStarted(_) | LifecycleStage::Active => {
                    inner.halt_pending = true;
                    inner
                        .external_request_queue
                        .push_back(ExternalRequest::Stop);
                    self.worker_state.cv.notify_one();
                    Ok(())
                }
                LifecycleStage::NoLongerActive => Ok(()),
            },

            // Requests to reboot require an active VM that isn't migrating or
            // halting. (Reboots during a migration are forbidden for
            // simplicity: until the migration is done, it's not clear whether
            // the source or target will be the one to handle the reboot
            // command.)
            ApiInstanceStateRequested::Reboot => {
                match inner.lifecycle_stage {
                    LifecycleStage::NotStarted(_) => {
                        Err(VmControllerError::InvalidStageForRequest(
                            requested,
                            inner.lifecycle_stage,
                        ))
                    }
                    LifecycleStage::Active => {
                        if inner.migrate_from_pending {
                            Err(VmControllerError::InvalidRequestForMigrationSource(requested))
                        } else if inner.halt_pending {
                            Err(VmControllerError::InstanceHaltPending)
                        } else {
                            inner
                                .external_request_queue
                                .push_back(ExternalRequest::Reboot);
                            self.worker_state.cv.notify_one();
                            Ok(())
                        }
                    }
                    LifecycleStage::NoLongerActive => {
                        Err(VmControllerError::InvalidStageForRequest(
                            requested,
                            inner.lifecycle_stage,
                        ))
                    }
                }
            }

            // Requests to mark the VM as a migration source require the VM to
            // be active.
            ApiInstanceStateRequested::MigrateStart => {
                match inner.lifecycle_stage {
                    LifecycleStage::NotStarted(_) => {
                        Err(VmControllerError::InvalidStageForRequest(
                            requested,
                            inner.lifecycle_stage,
                        ))
                    }
                    LifecycleStage::Active => {
                        inner.marked_migration_source = true;
                        Ok(())
                    }
                    LifecycleStage::NoLongerActive => {
                        Err(VmControllerError::InstanceNotActive)
                    }
                }
            }
        }
    }

    pub fn migrate_status(
        &self,
        migration_id: Uuid,
    ) -> Result<ApiMigrationState, MigrateError> {
        let inner = self.worker_state.inner.lock().unwrap();
        match inner.migration_state {
            None => Err(MigrateError::NoMigrationInProgress),
            Some((id, state)) => {
                if migration_id != id {
                    Err(MigrateError::UuidMismatch)
                } else {
                    Ok(state)
                }
            }
        }
    }

    fn for_each_entity<F>(&self, mut func: F) -> anyhow::Result<()>
    where
        F: FnMut(
            &Arc<dyn propolis::inventory::Entity>,
            &propolis::inventory::Record,
        ) -> anyhow::Result<()>,
    {
        self.instance().lock().inventory().for_each_node(
            propolis::inventory::Order::Pre,
            |_eid, record| -> Result<(), anyhow::Error> {
                let ent = record.entity();
                func(ent, record)
            },
        )
    }
}

impl Drop for VmController {
    fn drop(&mut self) {
        info!(self.log, "Dropping VM controller");
        let instance = self
            .vm_objects
            .instance
            .take()
            .expect("VM controller should have an instance at drop");
        drop(instance);

        // Send a final state monitor message indicating that the instance is
        // destroyed. Normally, the existence of this structure implies the
        // instance of at least one receiver, but at this point everything is
        // being dropped, so this call to `send` is not safe to unwrap.
        let api_state = &self.worker_state.inner.lock().unwrap().api_state;
        let gen = api_state.borrow().gen + 1;
        let _ = api_state.send(ApiMonitoredState {
            gen,
            state: ApiInstanceState::Destroyed,
        });
    }
}

/// An event that a VM's state driver must process.
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
    /// Sends a reset request to each entity in the instance, then sends a
    /// reset command to the instance's bhyve VM.
    fn reset_entities_and_machine(&self);

    /// Sends each entity a start request.
    fn start_entities(&self) -> anyhow::Result<()>;

    /// Sends each entity a pause request, then waits for all these requests to
    /// complete.
    fn pause_entities(&self);

    /// Sends each entity a resume request.
    fn resume_entities(&self);

    /// Sends each entity a halt request.
    fn halt_entities(&self);

    /// Resets the state of each vCPU in the instance to its on-reboot state.
    fn reset_vcpu_state(&self);

    /// Waits for a new event to arrive for processing and returns that event.
    fn wait_for_next_event(&self) -> StateDriverEvent;

    /// Sets the VM's current lifecycle stage.
    fn set_lifecycle_stage(&self, stage: LifecycleStage);

    /// Gets the VM's current lifecycle stage.
    fn get_lifecycle_stage(&self) -> LifecycleStage;

    /// Sets the VM's current migration state.
    fn set_migration_state(&self, migration_id: Uuid, state: ApiMigrationState);

    /// Gets the VM's current migration state.
    fn get_migration_state(&self) -> Option<(Uuid, ApiMigrationState)>;

    /// Updates the externally-visible VM state (i.e. the state visible through
    /// instance get and monitor commands).
    fn update_external_state(&self, gen: &mut u64, state: ApiInstanceState);

    /// Informs the controller that an attempt to migrate out of this VM
    /// completed with the supplied result.
    fn finish_migrate_as_source(&self, res: Result<(), MigrateError>);
}

impl StateDriverVmController for VmController {
    fn reset_entities_and_machine(&self) {
        self.for_each_entity(|ent, rec| {
            info!(self.log, "Sending reset request to {}", rec.name());
            ent.reset();
            Ok(())
        })
        .unwrap();

        self.instance().lock().machine().reinitialize().unwrap();
    }

    fn start_entities(&self) -> anyhow::Result<()> {
        self.for_each_entity(|ent, rec| {
            info!(self.log, "Sending startup complete to {}", rec.name());
            let res = ent.start();
            if let Err(e) = &res {
                error!(self.log, "Startup failed for {}: {:?}", rec.name(), e);
            }
            res
        })
    }

    fn pause_entities(&self) {
        self.for_each_entity(|ent, rec| {
            info!(self.log, "Sending pause request to {}", rec.name());
            ent.pause();
            Ok(())
        })
        .unwrap();

        info!(self.log, "Waiting for entities to pause");
        self.runtime_hdl.block_on(async {
            let mut devices = vec![];
            self.instance()
                .lock()
                .inventory()
                .for_each_node(
                    propolis::inventory::Order::Post,
                    |_eid, record| {
                        info!(
                            self.log,
                            "Got paused future from entity {}",
                            record.name()
                        );
                        devices.push(Arc::clone(record.entity()));
                        Ok::<_, ()>(())
                    },
                )
                .unwrap();

            let pause_futures = devices.iter().map(|ent| ent.paused());
            futures::future::join_all(pause_futures).await;
        });
    }

    fn resume_entities(&self) {
        self.for_each_entity(|ent, rec| {
            info!(self.log, "Sending resume request to {}", rec.name());
            ent.resume();
            Ok(())
        })
        .unwrap();
    }

    fn halt_entities(&self) {
        self.for_each_entity(|ent, rec| {
            info!(self.log, "Sending halt request to {}", rec.name());
            ent.halt();
            Ok(())
        })
        .unwrap();
    }

    fn reset_vcpu_state(&self) {
        for vcpu in self.instance().lock().machine().vcpus.iter() {
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

    fn wait_for_next_event(&self) -> StateDriverEvent {
        let guard = self.worker_state.inner.lock().unwrap();
        let mut guard = self
            .worker_state
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

    fn set_lifecycle_stage(&self, stage: LifecycleStage) {
        self.worker_state.inner.lock().unwrap().lifecycle_stage = stage;
    }

    fn get_lifecycle_stage(&self) -> LifecycleStage {
        self.worker_state.inner.lock().unwrap().lifecycle_stage
    }

    fn set_migration_state(
        &self,
        migration_id: Uuid,
        state: ApiMigrationState,
    ) {
        self.worker_state.inner.lock().unwrap().migration_state =
            Some((migration_id, state));
    }

    fn get_migration_state(&self) -> Option<(Uuid, ApiMigrationState)> {
        self.worker_state.inner.lock().unwrap().migration_state
    }

    fn update_external_state(&self, gen: &mut u64, state: ApiInstanceState) {
        self.worker_state
            .inner
            .lock()
            .unwrap()
            .update_external_state(gen, state);
    }

    fn finish_migrate_as_source(&self, res: Result<(), MigrateError>) {
        match res {
            Ok(()) => {
                info!(self.log, "Halting after successful migration out");

                let mut inner = self.worker_state.inner.lock().unwrap();
                inner.migrate_from_pending = false;
                inner.halt_pending = true;
                inner.external_request_queue.push_back(ExternalRequest::Stop);
            }
            Err(e) => {
                info!(self.log, "Resuming after failed migration out ({})", e);

                let mut inner = self.worker_state.inner.lock().unwrap();
                inner.migrate_from_pending = false;
                inner.lifecycle_stage = LifecycleStage::Active;
            }
        }
    }
}
