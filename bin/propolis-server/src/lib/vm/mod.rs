// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! This module implements the `Vm` wrapper type that encapsulates a single
//! instance on behalf of a Propolis server.

use std::{
    collections::BTreeMap,
    sync::{Arc, RwLock},
};

use oximeter::types::ProducerRegistry;
use propolis::{
    hw::{ps2::ctrl::PS2Ctrl, qemu::ramfb::RamFb, uart::LpcUart},
    vmm::Machine,
};
use propolis_api_types::{
    instance_spec::{v0::InstanceSpecV0, VersionedInstanceSpec},
    InstanceProperties, InstanceStateMonitorResponse, InstanceStateRequested,
};
use request_queue::ExternalRequest;
use rfb::server::VncServer;
use slog::info;
use uuid::Uuid;

use crate::{
    serial::Serial, server::MetricsEndpointConfig, vnc::PropolisVncServer,
};

pub(crate) mod guest_event;
mod lifecycle_ops;
pub(crate) mod migrate_commands;
mod request_queue;
mod services;
mod state_driver;

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
    #[error("VM ensure result channel unexpectedly closed")]
    EnsureResultClosed,

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
    inner: RwLock<VmInner>,
}

struct VmInner {
    state: VmState,
    driver: Option<tokio::task::JoinHandle<InstanceStateTx>>,
}

pub(crate) struct VmObjects {
    log: slog::Logger,
    instance_spec: InstanceSpecV0,
    machine: Machine,
    lifecycle_components: LifecycleMap,
    block_backends: BlockBackendMap,
    crucible_backends: CrucibleBackendMap,
    com1: Arc<Serial<LpcUart>>,
    framebuffer: Option<Arc<RamFb>>,
    ps2ctrl: Arc<PS2Ctrl>,
}

impl VmObjects {
    pub(crate) fn instance_spec(&self) -> &InstanceSpecV0 {
        &self.instance_spec
    }

    pub(crate) fn machine(&self) -> &Machine {
        &self.machine
    }

    pub(crate) fn device_by_name(
        &self,
        name: &str,
    ) -> Option<Arc<dyn propolis::common::Lifecycle>> {
        self.lifecycle_components.get(name).cloned()
    }

    pub(crate) fn block_backends(&self) -> &BlockBackendMap {
        &self.block_backends
    }

    pub(crate) fn crucible_backends(&self) -> &CrucibleBackendMap {
        &self.crucible_backends
    }

    pub(crate) fn com1(&self) -> &Arc<Serial<LpcUart>> {
        &self.com1
    }

    pub(crate) fn for_each_device(
        &self,
        mut func: impl FnMut(&str, &Arc<dyn propolis::common::Lifecycle>),
    ) {
        for (name, dev) in self.lifecycle_components.iter() {
            func(name, dev);
        }
    }

    pub(crate) fn for_each_device_fallible<E>(
        &self,
        mut func: impl FnMut(
            &str,
            &Arc<dyn propolis::common::Lifecycle>,
        ) -> std::result::Result<(), E>,
    ) -> std::result::Result<(), E> {
        for (name, dev) in self.lifecycle_components.iter() {
            func(name, dev)?;
        }

        Ok(())
    }
}

/// The state stored in a [`Vm`] when there is an actual underlying virtual
/// machine.
pub(super) struct ActiveVm {
    parent: Arc<Vm>,
    log: slog::Logger,

    state_driver_queue: Arc<state_driver::InputQueue>,
    external_state_rx: InstanceStateRx,

    properties: InstanceProperties,

    objects: Option<tokio::sync::RwLock<VmObjects>>,
    services: Option<services::VmServices>,
}

impl ActiveVm {
    pub(crate) fn log(&self) -> &slog::Logger {
        &self.log
    }

    pub(crate) async fn objects(
        &self,
    ) -> tokio::sync::RwLockReadGuard<'_, VmObjects> {
        self.objects.as_ref().unwrap().read().await
    }

    async fn objects_mut(
        &self,
    ) -> tokio::sync::RwLockWriteGuard<'_, VmObjects> {
        self.objects.as_ref().unwrap().write().await
    }

    pub(crate) fn put_state(
        &self,
        requested: InstanceStateRequested,
    ) -> Result<(), VmError> {
        info!(self.log, "requested state via API";
              "state" => ?requested);

        self.state_driver_queue
            .queue_external_request(match requested {
                InstanceStateRequested::Run => ExternalRequest::Start,
                InstanceStateRequested::Stop => ExternalRequest::Stop,
                InstanceStateRequested::Reboot => ExternalRequest::Reboot,
            })
            .map_err(Into::into)
    }

    pub(crate) fn reconfigure_crucible_volume(
        &self,
        disk_name: String,
        backend_id: Uuid,
        new_vcr_json: String,
        result_tx: CrucibleReplaceResultTx,
    ) -> Result<(), VmError> {
        self.state_driver_queue
            .queue_external_request(
                ExternalRequest::ReconfigureCrucibleVolume {
                    disk_name,
                    backend_id,
                    new_vcr_json,
                    result_tx,
                },
            )
            .map_err(Into::into)
    }

    pub(crate) fn services(&self) -> &services::VmServices {
        self.services.as_ref().expect("active VMs always have services")
    }
}

impl Drop for ActiveVm {
    fn drop(&mut self) {
        let driver = self
            .parent
            .inner
            .write()
            .unwrap()
            .driver
            .take()
            .expect("active VMs always have a driver");

        let objects =
            self.objects.take().expect("active VMs should always have objects");

        let services = self
            .services
            .take()
            .expect("active VMs should always have services");

        let parent = self.parent.clone();
        let log = self.log.clone();
        tokio::spawn(async move {
            drop(objects);
            services.stop(&log).await;

            let tx = driver.await.expect("state driver shouldn't panic");
            let new_state = {
                let old_state = tx.borrow();
                InstanceStateMonitorResponse {
                    gen: old_state.gen + 1,
                    state: propolis_api_types::InstanceState::Destroyed,
                    migration: old_state.migration.clone(),
                }
            };

            tx.send(new_state).expect("VM in rundown should hold a receiver");

            parent.complete_rundown().await;
        });
    }
}

struct RundownVm {
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

    /// There is an active state driver task, but it is currently creating VM
    /// components and/or starting VM services.
    WaitingForInit,

    /// There is an active virtual machine
    Active(Arc<ActiveVm>),

    /// The active VM's state driver has exited, but the
    Rundown(RundownVm),
    RundownComplete(RundownVm),
}

pub(super) struct EnsureOptions {
    pub toml_config: Arc<crate::server::VmTomlConfig>,
    pub use_reservoir: bool,
    pub metrics_config: Option<MetricsEndpointConfig>,
    pub oximeter_registry: Option<ProducerRegistry>,
    pub nexus_client: Option<nexus_client::Client>,
    pub vnc_server: Arc<VncServer<PropolisVncServer>>,
}

impl Vm {
    pub fn new() -> Arc<Self> {
        let inner = VmInner { state: VmState::NoVm, driver: None };
        Arc::new(Self { inner: RwLock::new(inner) })
    }

    pub(super) fn active_vm(&self) -> Option<Arc<ActiveVm>> {
        let guard = self.inner.read().unwrap();
        if let VmState::Active(vm) = &guard.state {
            Some(vm.clone())
        } else {
            None
        }
    }

    pub(super) async fn get(
        &self,
    ) -> Result<propolis_api_types::InstanceSpecGetResponse, VmError> {
        let vm = match &self.inner.read().unwrap().state {
            VmState::NoVm => {
                return Err(VmError::NotCreated);
            }
            VmState::WaitingForInit => {
                return Err(VmError::WaitingToInitialize);
            }
            VmState::Active(vm) => vm.clone(),
            VmState::Rundown(vm) | VmState::RundownComplete(vm) => {
                return Ok(propolis_api_types::InstanceSpecGetResponse {
                    properties: vm.properties.clone(),
                    state: vm.external_state_rx.borrow().state,
                    spec: VersionedInstanceSpec::V0(vm.spec.clone()),
                });
            }
        };

        let spec = vm.objects().await.instance_spec().clone();
        let state = vm.external_state_rx.borrow().clone();
        Ok(propolis_api_types::InstanceSpecGetResponse {
            properties: vm.properties.clone(),
            spec: VersionedInstanceSpec::V0(spec),
            state: state.state,
        })
    }

    pub(super) fn state_watcher(&self) -> Result<InstanceStateRx, VmError> {
        let guard = self.inner.read().unwrap();
        match &guard.state {
            VmState::NoVm => Err(VmError::NotCreated),
            VmState::WaitingForInit => Err(VmError::WaitingToInitialize),
            VmState::Active(vm) => Ok(vm.external_state_rx.clone()),
            VmState::Rundown(vm) | VmState::RundownComplete(vm) => {
                Ok(vm.external_state_rx.clone())
            }
        }
    }

    fn make_active(&self, active: Arc<ActiveVm>) {
        let mut guard = self.inner.write().unwrap();
        let old = std::mem::replace(&mut guard.state, VmState::NoVm);
        match old {
            VmState::WaitingForInit => {
                guard.state = VmState::Active(active.clone());
            }
            _ => unreachable!(
                "only a starting VM's state worker calls make_active"
            ),
        }
    }

    async fn set_rundown(&self) {
        let vm = self
            .active_vm()
            .expect("VM should be active before being run down");

        let new_state = VmState::Rundown(RundownVm {
            external_state_rx: vm.external_state_rx.clone(),
            properties: vm.properties.clone(),
            spec: vm.objects().await.instance_spec.clone(),
        });

        self.inner.write().unwrap().state = new_state;
    }

    async fn complete_rundown(&self) {
        let mut guard = self.inner.write().unwrap();
        let old = std::mem::replace(&mut guard.state, VmState::NoVm);
        match old {
            VmState::Rundown(vm) => guard.state = VmState::RundownComplete(vm),
            _ => unreachable!("VM rundown completed from invalid prior state"),
        }
    }

    pub(crate) async fn ensure(
        self: &Arc<Self>,
        log: slog::Logger,
        ensure_request: propolis_api_types::InstanceSpecEnsureRequest,
        options: EnsureOptions,
    ) -> anyhow::Result<propolis_api_types::InstanceEnsureResponse, VmError>
    {
        let (ensure_reply_tx, ensure_rx) = tokio::sync::oneshot::channel();

        // Take the lock for writing, since in the common case this call will be
        // creating a new VM and there's no easy way to upgrade from a reader
        // lock to a writer lock.
        {
            let mut guard = self.inner.write().unwrap();
            match guard.state {
                VmState::WaitingForInit => {
                    return Err(VmError::WaitingToInitialize)
                }
                VmState::Active(_) => return Err(VmError::AlreadyInitialized),
                VmState::Rundown(_) => return Err(VmError::RundownInProgress),
                _ => {}
            }

            guard.state = VmState::WaitingForInit;
            let vm_for_driver = self.clone();
            guard.driver = Some(tokio::spawn(async move {
                state_driver::run_state_driver(
                    log,
                    vm_for_driver,
                    ensure_request,
                    ensure_reply_tx,
                    options,
                )
                .await
            }));
        }

        ensure_rx.await.map_err(|_| VmError::EnsureResultClosed)?
    }
}
