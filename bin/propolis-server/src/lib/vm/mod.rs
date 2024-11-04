// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Implements the [`Vm`] type, which encapsulates a single Propolis virtual
//! machine instance and provides a public interface thereto to the Propolis
//! Dropshot server.
//!
//! The VM state machine looks like this:
//!
//! ```text
//!            [NoVm]
//!              |
//!              |
//!              v
//! +---- WaitingForInit <----+
//! |            |            |
//! |            |            |
//! |            v            |
//! |         Active          |
//! |            |            |
//! |            |            |
//! |            v            |
//! +-------> Rundown         |
//! |            |            |
//! |            |            |
//! |            v            |
//! +---> RundownComplete ----+
//! ```
//!
//! In the happy case where new VMs always start successfully, this state
//! machine transitions as follows:
//!
//! - New state machines start in [`VmState::NoVm`].
//! - A request to create a new VM moves to [`VmState::WaitingForInit`].
//! - Once all of the VM's components are created, the VM moves to
//!   [`VmState::Active`].
//! - When the VM stops, the VM moves to [`VmState::Rundown`].
//! - When all references to the VM's components are dropped, the VM moves to
//!   [`VmState::RundownComplete`]. A request to create a new VM will move back
//!   to `WaitingForInit`.
//!
//! In any state except `NoVm`, the state machine holds enough state to describe
//! the most recent VM known to the state machine, whether it is being created
//! (`WaitingForInit`), running (`Active`), or being torn down (`Rundown` and
//! `RundownComplete`).
//!
//! In the `Active` state, the VM wrapper holds an [`active::ActiveVm`] and
//! allows API-layer callers to obtain references to it. These callers use these
//! references to ask to change a VM's state or change its configuration. An
//! active VM holds a reference to a [`objects::VmObjects`] structure that
//! bundles up all of the Propolis components (kernel VM, devices, and backends)
//! that make up an instance and a spec that describes that instance; API-layer
//! callers may use this structure to read the instance's properties and query
//! component state, but cannot mutate the VM's structure this way.
//!
//! Requests to change a VM's state or configuration (and events from a running
//! guest that might change a VM's state, like an in-guest shutdown or reboot
//! request or a triple fault) are placed in an [input
//! queue](state_driver::InputQueue) that is serviced by a single "state driver"
//! task. When an instance stops, this task moves the state machine to the
//! `Rundown` state, which renders new API-layer callers unable to clone new
//! references to the VM's `VmObjects`. When all outstanding references to the
//! objects are dropped, the VM moves to the `RundownComplete` state, obtains
//! the final instance state from the (joined) state driver task, and publishes
//! that state. At that point the VM may be reinitialized.
//!
//! The VM state machine delegates VM creation to the state driver task. This
//! task can fail to initialize a VM in two ways:
//!
//! 1. It may fail to create all of the VM's component objects (e.g. due to
//!    bad configuration or resource exhaustion).
//! 2. It may successfully create all of the VM's component objects, but then
//!    fail to populate their initial state via live migration from another
//!    instance.
//!
//! In the former case, where no VM objects are ever created, the state driver
//! moves the state machine directly from `WaitingForInit` to `RundownComplete`.
//! In the latter case, the driver moves to `Rundown` and allows `VmObjects`
//! teardown to drive the state machine to `RundownComplete`.

use std::{collections::BTreeMap, net::SocketAddr, path::PathBuf, sync::Arc};

use active::ActiveVm;
use ensure::{VmEnsureRequest, VmInitializationMethod};
use oximeter::types::ProducerRegistry;
use propolis_api_types::{
    instance_spec::{SpecKey, VersionedInstanceSpec},
    InstanceEnsureResponse, InstanceMigrateStatusResponse,
    InstanceMigrationStatus, InstanceProperties, InstanceSpecGetResponse,
    InstanceState, InstanceStateMonitorResponse, MigrationState,
};
use slog::info;
use state_driver::StateDriverOutput;
use state_publisher::StatePublisher;
use tokio::sync::{oneshot, watch, RwLock, RwLockReadGuard};

use crate::{server::MetricsEndpointConfig, spec::Spec, vnc::VncServer};

mod active;
pub(crate) mod ensure;
pub(crate) mod guest_event;
pub(crate) mod objects;
mod request_queue;
mod services;
mod state_driver;
pub(crate) mod state_publisher;

/// Maps component names to lifecycle trait objects that allow
/// components to be started, paused, resumed, and halted.
pub(crate) type DeviceMap =
    BTreeMap<SpecKey, Arc<dyn propolis::common::Lifecycle>>;

/// Mapping of NIC identifiers to viona device instance IDs.
/// We use a Vec here due to the limited size of the NIC array.
pub(crate) type NetworkInterfaceIds = Vec<(uuid::Uuid, KstatInstanceId)>;

/// Maps component names to block backend trait objects.
pub(crate) type BlockBackendMap =
    BTreeMap<SpecKey, Arc<dyn propolis::block::Backend>>;

/// Maps disk IDs to Crucible backend objects.
pub(crate) type CrucibleBackendMap =
    BTreeMap<SpecKey, Arc<propolis::block::CrucibleBackend>>;

/// Type alias for the sender side of the channel that receives
/// externally-visible instance state updates.
type InstanceStateTx = watch::Sender<InstanceStateMonitorResponse>;

/// Type alias for the receiver side of the channel that receives
/// externally-visible instance state updates.
type InstanceStateRx = watch::Receiver<InstanceStateMonitorResponse>;

/// Type alias for the results sent by the state driver in response to a request
/// to change a Crucible backend's configuration.
pub(crate) type CrucibleReplaceResult =
    Result<crucible_client_types::ReplaceResult, dropshot::HttpError>;

/// Type alias for the sender side of a channel that receives Crucible backend
/// reconfiguration results.
pub(crate) type CrucibleReplaceResultTx =
    oneshot::Sender<CrucibleReplaceResult>;

/// PCI device instance ID type to which a per-component Kstat (kernal stat)
/// instance ID maps to.
type KstatInstanceId = u32;

/// Type alias for the sender side of a channel that receives the results of
/// instance-ensure API calls.
type InstanceEnsureResponseTx =
    oneshot::Sender<Result<InstanceEnsureResponse, VmError>>;

/// The minimum number of threads to spawn in the Tokio runtime that runs the
/// state driver and any other VM-related tasks.
const VMM_MIN_RT_THREADS: usize = 8;

/// When creating a new VM, add the VM's vCPU count to this value, then spawn
/// that many threads on its Tokio runtime or [`VMM_MIN_RT_THREADS`], whichever
/// is greater.
const VMM_BASE_RT_THREADS: usize = 4;

/// Errors generated by the VM controller and its subcomponents.
#[derive(Debug, thiserror::Error)]
pub(crate) enum VmError {
    #[error("VM operation result channel unexpectedly closed")]
    ResultChannelClosed,

    #[error("VM is currently initializing")]
    WaitingToInitialize,

    #[error("VM already initialized")]
    AlreadyInitialized,

    #[error("VM is currently shutting down")]
    RundownInProgress,

    #[error("VM initialization failed: {0}")]
    InitializationFailed(String),

    #[error("Forbidden state change")]
    ForbiddenStateChange(#[from] request_queue::RequestDeniedReason),
}

/// The top-level VM wrapper type.
pub(crate) struct Vm {
    /// Lock wrapper for the VM state machine's contents.
    ///
    /// Routines that need to read VM properties or obtain a `VmObjects` handle
    /// acquire this lock shared.
    ///
    /// Routines that drive the VM state machine acquire this lock exclusively.
    inner: RwLock<VmInner>,

    /// A logger for this VM.
    log: slog::Logger,
}

/// Holds a VM state machine and state driver task handle.
struct VmInner {
    /// The VM's current state.
    state: VmState,

    /// A handle to the VM's current state driver task, if it has one.
    driver: Option<tokio::task::JoinHandle<StateDriverOutput>>,
}

/// Describes a past or future VM and its properties.
struct VmDescription {
    /// Records the VM's last externally-visible state.
    external_state_rx: InstanceStateRx,

    /// The VM's API-level instance properties.
    properties: InstanceProperties,

    /// The VM's last-known instance specification, or None if no specification
    /// has yet been supplied for this VM.
    spec: Option<Spec>,

    /// The runtime on which the VM's state driver is running (or on which it
    /// ran).
    ///
    /// This is preserved in the VM state machine so that the state driver task
    /// doesn't drop its runtime out from under itself when it signals that the
    /// state machine should transition from Active to Rundown.
    tokio_rt: Option<tokio::runtime::Runtime>,
}

/// The states in the VM state machine. See the module comment for more details.
#[allow(clippy::large_enum_variant)]
enum VmState {
    /// This state machine has never held a VM.
    NoVm,

    /// A new state driver is attempting to initialize objects for a VM with the
    /// ecnlosed description.
    WaitingForInit(VmDescription),

    /// The VM is active, and callers can obtain a handle to its objects.
    Active(active::ActiveVm),

    /// The previous VM is shutting down, but its objects have not been fully
    /// destroyed yet.
    Rundown(VmDescription),

    /// The previous VM and its objects have been cleaned up.
    RundownComplete(VmDescription),
}

impl std::fmt::Display for VmState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}",
            match self {
                Self::NoVm => "NoVm",
                Self::WaitingForInit(_) => "WaitingForInit",
                Self::Active(_) => "Active",
                Self::Rundown(_) => "Rundown",
                Self::RundownComplete(_) => "RundownComplete",
            }
        )
    }
}

/// Parameters to an instance ensure operation.
pub(super) struct EnsureOptions {
    /// A reference to this server process's bootrom path.
    pub(super) bootrom_path: Arc<PathBuf>,

    /// The bootrom version string to display to the guest.
    pub(super) bootrom_version: Option<String>,

    /// True if VMs should allocate memory from the kernel VMM reservoir.
    pub(super) use_reservoir: bool,

    /// Configuration used to serve Oximeter metrics from this server.
    pub(super) metrics_config: Option<MetricsEndpointConfig>,

    /// An Oximeter producer registry to pass to components that will emit
    /// Oximeter metrics.
    pub(super) oximeter_registry: Option<ProducerRegistry>,

    /// A Nexus client handle to pass to components that can make upcalls to
    /// Nexus.
    pub(super) nexus_client: Option<nexus_client::Client>,

    /// A reference to the process's VNC server, used to connect the server to
    /// a new VM's framebuffer.
    pub(super) vnc_server: Arc<VncServer>,

    /// The address of this Propolis process, used by the live migration
    /// protocol to transfer serial console connections.
    pub(super) local_server_addr: SocketAddr,
}

impl Vm {
    /// Creates a new VM.
    pub fn new(log: &slog::Logger) -> Arc<Self> {
        let log = log.new(slog::o!("component" => "vm_wrapper"));
        let inner = VmInner { state: VmState::NoVm, driver: None };
        Arc::new(Self { inner: RwLock::new(inner), log })
    }

    /// If the VM is `Active`, yields a shared lock guard with a reference to
    /// the relevant `ActiveVm`. Returns `None` if there is no active VM.
    pub(super) async fn active_vm(
        &self,
    ) -> Option<RwLockReadGuard<'_, ActiveVm>> {
        RwLockReadGuard::try_map(self.inner.read().await, |inner| {
            if let VmState::Active(vm) = &inner.state {
                Some(vm)
            } else {
                None
            }
        })
        .ok()
    }

    /// Returns the state, properties, and instance spec for the instance most
    /// recently wrapped by this `Vm`.
    ///
    /// # Returns
    ///
    /// - `Some` if the VM has been created.
    /// - `None` if no VM has ever been created.
    pub(super) async fn get(&self) -> Option<InstanceSpecGetResponse> {
        let guard = self.inner.read().await;
        match &guard.state {
            // If no VM has ever been created, there's nothing to get.
            VmState::NoVm => None,

            // If the VM is active, pull the required data out of its objects.
            VmState::Active(vm) => {
                let spec =
                    vm.objects().lock_shared().await.instance_spec().clone();
                let state = vm.external_state_rx.borrow().clone();
                Some(InstanceSpecGetResponse {
                    properties: vm.properties.clone(),
                    spec: Some(VersionedInstanceSpec::V0(spec.into())),
                    state: state.state,
                })
            }

            // If the VM is not active yet, or there is only a
            // previously-run-down VM, return the state saved in the state
            // machine.
            VmState::WaitingForInit(vm)
            | VmState::Rundown(vm)
            | VmState::RundownComplete(vm) => Some(InstanceSpecGetResponse {
                properties: vm.properties.clone(),
                state: vm.external_state_rx.borrow().state,
                spec: vm
                    .spec
                    .clone()
                    .map(|s| VersionedInstanceSpec::V0(s.into())),
            }),
        }
    }

    /// Yields a handle to the most recent instance state receiver wrapped by
    /// this `Vm`.
    ///
    /// # Returns
    ///
    /// - `Some` if the VM has been created.
    /// - `None` if no VM has ever been created.
    pub(super) async fn state_watcher(&self) -> Option<InstanceStateRx> {
        let guard = self.inner.read().await;
        match &guard.state {
            VmState::NoVm => None,
            VmState::Active(vm) => Some(vm.external_state_rx.clone()),
            VmState::WaitingForInit(vm)
            | VmState::Rundown(vm)
            | VmState::RundownComplete(vm) => {
                Some(vm.external_state_rx.clone())
            }
        }
    }

    /// Moves this VM from the `WaitingForInit` state to the `Active` state,
    /// creating an `ActiveVm` with the supplied input queue, VM objects, and VM
    /// services.
    ///
    /// # Panics
    ///
    /// Panics if the VM is not in the `WaitingForInit` state.
    async fn make_active(
        self: &Arc<Self>,
        log: &slog::Logger,
        state_driver_queue: Arc<state_driver::InputQueue>,
        objects: &Arc<objects::VmObjects>,
        vmm_rt: tokio::runtime::Runtime,
        services: services::VmServices,
    ) {
        info!(self.log, "installing active VM");
        let mut guard = self.inner.write().await;
        let old = std::mem::replace(&mut guard.state, VmState::NoVm);
        match old {
            VmState::WaitingForInit(vm) => {
                guard.state = VmState::Active(ActiveVm {
                    log: log.clone(),
                    state_driver_queue,
                    external_state_rx: vm.external_state_rx,
                    properties: vm.properties,
                    objects: objects.clone(),
                    services,
                    tokio_rt: vmm_rt,
                });
            }
            state => unreachable!(
                "only a starting VM's state worker calls make_active \
                (current state: {state})"
            ),
        }
    }

    /// Moves this VM from the `WaitingForInit` state to the `RundownComplete`
    /// state in response to an instance initialization failure.
    ///
    /// The caller must ensure there are no active `VmObjects` that refer to
    /// this VM.
    ///
    /// # Panics
    ///
    /// Panics if the VM is not in the `WaitingForInit` state.
    async fn vm_init_failed(&self) {
        let mut guard = self.inner.write().await;
        let old = std::mem::replace(&mut guard.state, VmState::NoVm);
        match old {
            VmState::WaitingForInit(vm) => {
                guard.state = VmState::RundownComplete(vm)
            }
            state => unreachable!(
                "start failures should only occur before an active VM is \
                installed (current state: {state})"
            ),
        }
    }

    /// Moves this VM from the `Active` state to the `Rundown` state.
    ///
    /// This routine should only be called by the state driver.
    ///
    /// # Panics
    ///
    /// Panics if the VM is not in the `Active` state.
    async fn set_rundown(&self) {
        info!(self.log, "setting VM rundown");
        let services = {
            let mut guard = self.inner.write().await;
            let old = std::mem::replace(&mut guard.state, VmState::NoVm);
            let vm = match old {
                VmState::Active(vm) => vm,
                state => panic!(
                    "VM should be active before being run down (current state: \
                    {state})"
                ),
            };

            let spec = vm.objects().lock_shared().await.instance_spec().clone();
            let ActiveVm { external_state_rx, properties, tokio_rt, .. } = vm;
            guard.state = VmState::Rundown(VmDescription {
                external_state_rx,
                properties,
                spec: Some(spec),
                tokio_rt: Some(tokio_rt),
            });
            vm.services
        };

        services.stop(&self.log).await;
    }

    /// Moves this VM from the `Rundown` state to the `RundownComplete` state.
    ///
    /// This routine should only be called when dropping VM objects.
    ///
    /// # Panics
    ///
    /// Panics if the VM is not in the `Rundown` state.
    async fn complete_rundown(&self) {
        info!(self.log, "completing VM rundown");
        let mut guard = self.inner.write().await;
        let old = std::mem::replace(&mut guard.state, VmState::NoVm);
        let rt = match old {
            VmState::Rundown(mut vm) => {
                let rt = vm.tokio_rt.take().expect("rundown VM has a runtime");
                guard.state = VmState::RundownComplete(vm);
                rt
            }
            state => unreachable!(
                "VM rundown completed from invalid prior state {state}"
            ),
        };

        let StateDriverOutput { mut state_publisher, final_state } = guard
            .driver
            .take()
            .expect("driver must exist in rundown")
            .await
            .expect("state driver shouldn't panic");

        state_publisher.update(state_publisher::ExternalStateUpdate::Instance(
            final_state,
        ));

        // Shut down the runtime without blocking to wait for tasks to complete
        // (since blocking is illegal in an async context).
        //
        // This must happen after the state driver task has successfully joined
        // (otherwise it might be canceled and will fail to yield the VM's final
        // state).
        rt.shutdown_background();
    }

    /// Attempts to move this VM to the `Active` state by setting up a state
    /// driver task and directing it to initialize a new VM.
    pub(crate) async fn ensure(
        self: &Arc<Self>,
        log: &slog::Logger,
        ensure_request: VmEnsureRequest,
        options: EnsureOptions,
    ) -> Result<InstanceEnsureResponse, VmError> {
        let log_for_driver =
            log.new(slog::o!("component" => "vm_state_driver"));

        // This routine will create a state driver task that actually
        // initializes the VM. The external instance-ensure API shouldn't return
        // until that task has disposed of the initialization request. Create a
        // channel to allow the state driver task to send back an ensure result
        // at the appropriate moment.
        let (ensure_reply_tx, ensure_rx) = oneshot::channel();

        // The external state receiver needs to exist as soon as this routine
        // returns, so create the appropriate channel here. The sender side of
        // the channel will move to the state driver task.
        let (external_publisher, external_rx) = StatePublisher::new(
            &log_for_driver,
            InstanceStateMonitorResponse {
                gen: 1,
                state: match ensure_request.init {
                    VmInitializationMethod::Spec(_) => InstanceState::Creating,
                    VmInitializationMethod::Migration(_) => {
                        InstanceState::Migrating
                    }
                },
                migration: InstanceMigrateStatusResponse {
                    migration_in: match &ensure_request.init {
                        VmInitializationMethod::Spec(_) => None,
                        VmInitializationMethod::Migration(info) => {
                            Some(InstanceMigrationStatus {
                                id: info.migration_id,
                                state: MigrationState::Sync,
                            })
                        }
                    },
                    migration_out: None,
                },
            },
        );

        // Take the lock for writing, since in the common case this call will be
        // creating a new VM and there's no easy way to upgrade from a reader
        // lock to a writer lock.
        {
            let mut guard = self.inner.write().await;
            match guard.state {
                VmState::WaitingForInit(_) => {
                    return Err(VmError::WaitingToInitialize);
                }
                VmState::Active(_) => return Err(VmError::AlreadyInitialized),
                VmState::Rundown(_) => return Err(VmError::RundownInProgress),
                _ => {}
            };

            let properties = ensure_request.properties.clone();
            let spec = ensure_request.spec().cloned();
            let vm_for_driver = self.clone();
            guard.driver = Some(tokio::spawn(async move {
                state_driver::run_state_driver(
                    log_for_driver,
                    vm_for_driver,
                    external_publisher,
                    ensure_request,
                    ensure_reply_tx,
                    options,
                )
                .await
            }));

            guard.state = VmState::WaitingForInit(VmDescription {
                external_state_rx: external_rx.clone(),
                properties,
                spec,
                tokio_rt: None,
            });
        }

        // Wait for the state driver task to dispose of this request.
        ensure_rx.await.map_err(|_| VmError::ResultChannelClosed)?
    }
}
