// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! This module implements the `Vm` wrapper type that encapsulates a single
//! instance on behalf of a Propolis server.

use std::{collections::BTreeMap, net::SocketAddr, sync::Arc};

use active::ActiveVm;
use oximeter::types::ProducerRegistry;
use propolis_api_types::{
    instance_spec::{v0::InstanceSpecV0, VersionedInstanceSpec},
    InstanceProperties,
};
use rfb::server::VncServer;
use slog::info;
use state_publisher::StatePublisher;

use crate::{server::MetricsEndpointConfig, vnc::PropolisVncServer};

mod active;
pub(crate) mod guest_event;
pub(crate) mod migrate_commands;
pub(crate) mod objects;
mod request_queue;
mod services;
mod state_driver;
mod state_publisher;

pub(crate) type LifecycleMap =
    BTreeMap<String, Arc<dyn propolis::common::Lifecycle>>;
pub(crate) type BlockBackendMap =
    BTreeMap<String, Arc<dyn propolis::block::Backend>>;
pub(crate) type CrucibleBackendMap =
    BTreeMap<uuid::Uuid, Arc<propolis::block::CrucibleBackend>>;

type InstanceStateTx = tokio::sync::watch::Sender<
    propolis_api_types::InstanceStateMonitorResponse,
>;
type InstanceStateRx = tokio::sync::watch::Receiver<
    propolis_api_types::InstanceStateMonitorResponse,
>;

pub(crate) type CrucibleReplaceResult =
    Result<crucible_client_types::ReplaceResult, dropshot::HttpError>;
pub(crate) type CrucibleReplaceResultTx =
    tokio::sync::oneshot::Sender<CrucibleReplaceResult>;

#[derive(Debug, thiserror::Error)]
pub(crate) enum VmError {
    #[error("VM operation result channel unexpectedly closed")]
    ResultChannelClosed,

    #[error("VM not created")]
    NotCreated,

    #[error("VM is currently initializing")]
    WaitingToInitialize,

    #[error("VM already initialized")]
    AlreadyInitialized,

    #[error("VM is currently shutting down")]
    RundownInProgress,

    #[error("VM initialization failed")]
    InitializationFailed(#[source] anyhow::Error),

    #[error("Forbidden state change")]
    ForbiddenStateChange(#[from] request_queue::RequestDeniedReason),
}

/// The top-level VM wrapper type. Callers are expected to wrap this in an
/// `Arc`.
pub(crate) struct Vm {
    inner: tokio::sync::RwLock<VmInner>,
    log: slog::Logger,
}

struct VmInner {
    state: VmState,
    driver: Option<tokio::task::JoinHandle<StatePublisher>>,
}

struct UninitVm {
    external_state_rx: InstanceStateRx,
    properties: InstanceProperties,
    spec: InstanceSpecV0,
}

/// An enum representing the VM state machine. The API layer's Dropshot context
/// holds a reference to this state machine via the [`Vm`] wrapper struct.
///
/// When an instance is running, its components and services are stored in an
/// [`ActiveVm`] whose lifecycle is managed by a "state driver" task. The VM is
/// kept alive by this task's strong reference. API calls that need to access
/// the active VM try to upgrade the state machine's weak reference to the VM.
///
/// When an active VM halts, the state driver moves the state machine to the
/// `Rundown` state, preventing new API calls from obtaining new strong
/// references to the underlying VM while allowing existing calls to finish.
/// Eventually (barring a leak), the active VM will be dropped. This launches a
/// task that finishes cleaning up the VM and then moves to the
/// `RundownComplete` state, which allows a new VM to start.
#[allow(clippy::large_enum_variant)]
enum VmState {
    /// This state machine has never held a VM.
    NoVm,
    WaitingForInit(UninitVm),
    Active(active::ActiveVm),
    Rundown(UninitVm),
    RundownComplete(UninitVm),
}

pub(super) struct EnsureOptions {
    pub toml_config: Arc<crate::server::VmTomlConfig>,
    pub use_reservoir: bool,
    pub metrics_config: Option<MetricsEndpointConfig>,
    pub oximeter_registry: Option<ProducerRegistry>,
    pub nexus_client: Option<nexus_client::Client>,
    pub vnc_server: Arc<VncServer<PropolisVncServer>>,
    pub local_server_addr: SocketAddr,
}

impl Vm {
    pub fn new(log: &slog::Logger) -> Arc<Self> {
        let log = log.new(slog::o!("component" => "vm_wrapper"));
        let inner = VmInner { state: VmState::NoVm, driver: None };
        Arc::new(Self { inner: tokio::sync::RwLock::new(inner), log })
    }

    pub(super) async fn active_vm(
        &self,
    ) -> Option<tokio::sync::RwLockReadGuard<'_, ActiveVm>> {
        tokio::sync::RwLockReadGuard::try_map(
            self.inner.read().await,
            |inner| {
                if let VmState::Active(vm) = &inner.state {
                    Some(vm)
                } else {
                    None
                }
            },
        )
        .ok()
    }

    pub(super) async fn get(
        &self,
    ) -> Result<propolis_api_types::InstanceSpecGetResponse, VmError> {
        let guard = self.inner.read().await;
        let vm = match &guard.state {
            VmState::NoVm => {
                return Err(VmError::NotCreated);
            }
            VmState::Active(vm) => vm,
            VmState::WaitingForInit(vm)
            | VmState::Rundown(vm)
            | VmState::RundownComplete(vm) => {
                return Ok(propolis_api_types::InstanceSpecGetResponse {
                    properties: vm.properties.clone(),
                    state: vm.external_state_rx.borrow().state,
                    spec: VersionedInstanceSpec::V0(vm.spec.clone()),
                });
            }
        };

        let spec = vm.objects().read().await.instance_spec().clone();
        let state = vm.external_state_rx.borrow().clone();
        Ok(propolis_api_types::InstanceSpecGetResponse {
            properties: vm.properties.clone(),
            spec: VersionedInstanceSpec::V0(spec),
            state: state.state,
        })
    }

    pub(super) async fn state_watcher(
        &self,
    ) -> Result<InstanceStateRx, VmError> {
        let guard = self.inner.read().await;
        match &guard.state {
            VmState::NoVm => Err(VmError::NotCreated),
            VmState::Active(vm) => Ok(vm.external_state_rx.clone()),
            VmState::WaitingForInit(vm)
            | VmState::Rundown(vm)
            | VmState::RundownComplete(vm) => Ok(vm.external_state_rx.clone()),
        }
    }

    async fn make_active(
        self: &Arc<Self>,
        log: &slog::Logger,
        state_driver_queue: Arc<state_driver::InputQueue>,
        objects: &Arc<objects::VmObjects>,
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
                });
            }
            _ => unreachable!(
                "only a starting VM's state worker calls make_active"
            ),
        }
    }

    async fn start_failed(&self) {
        let mut guard = self.inner.write().await;
        let old = std::mem::replace(&mut guard.state, VmState::NoVm);
        match old {
            VmState::WaitingForInit(vm) => {
                guard.state = VmState::RundownComplete(vm)
            }
            _ => unreachable!(
                "start failures should only occur before an active VM is installed")
        }
    }

    async fn set_rundown(&self) {
        info!(self.log, "setting VM rundown");
        let services = {
            let mut guard = self.inner.write().await;
            let VmState::Active(vm) =
                std::mem::replace(&mut guard.state, VmState::NoVm)
            else {
                panic!("VM should be active before being run down");
            };

            let spec = vm.objects().read().await.instance_spec().clone();
            let ActiveVm { external_state_rx, properties, .. } = vm;
            guard.state = VmState::Rundown(UninitVm {
                external_state_rx,
                properties,
                spec,
            });
            vm.services
        };

        services.stop(&self.log).await;
    }

    async fn complete_rundown(&self) {
        info!(self.log, "completing VM rundown");
        let mut guard = self.inner.write().await;
        let old = std::mem::replace(&mut guard.state, VmState::NoVm);
        match old {
            VmState::WaitingForInit(vm) | VmState::Rundown(vm) => {
                guard.state = VmState::RundownComplete(vm)
            }
            _ => unreachable!("VM rundown completed from invalid prior state"),
        }
    }

    pub(crate) async fn ensure(
        self: &Arc<Self>,
        log: &slog::Logger,
        ensure_request: propolis_api_types::InstanceSpecEnsureRequest,
        options: EnsureOptions,
    ) -> Result<propolis_api_types::InstanceEnsureResponse, VmError> {
        let log_for_driver =
            log.new(slog::o!("component" => "vm_state_driver"));

        let (ensure_reply_tx, ensure_rx) = tokio::sync::oneshot::channel();
        let (external_publisher, external_rx) = StatePublisher::new(
            &log_for_driver,
            propolis_api_types::InstanceStateMonitorResponse {
                gen: 1,
                state: if ensure_request.migrate.is_some() {
                    propolis_api_types::InstanceState::Migrating
                } else {
                    propolis_api_types::InstanceState::Creating
                },
                migration: propolis_api_types::InstanceMigrateStatusResponse {
                    migration_in: ensure_request.migrate.as_ref().map(|req| {
                        propolis_api_types::InstanceMigrationStatus {
                            id: req.migration_id,
                            state: propolis_api_types::MigrationState::Sync,
                        }
                    }),
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
                    return Err(VmError::WaitingToInitialize)
                }
                VmState::Active(_) => return Err(VmError::AlreadyInitialized),
                VmState::Rundown(_) => return Err(VmError::RundownInProgress),
                _ => {}
            }

            let VersionedInstanceSpec::V0(v0_spec) =
                ensure_request.instance_spec.clone();
            guard.state = VmState::WaitingForInit(UninitVm {
                external_state_rx: external_rx.clone(),
                properties: ensure_request.properties.clone(),
                spec: v0_spec,
            });

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
        }

        ensure_rx.await.map_err(|_| VmError::ResultChannelClosed)?
    }
}
