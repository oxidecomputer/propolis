// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! This module implements the `Vm` wrapper type that encapsulates a single
//! instance on behalf of a Propolis server.

use std::{
    collections::BTreeMap,
    sync::{Arc, RwLock, RwLockReadGuard, Weak},
};

use oximeter::types::ProducerRegistry;
use propolis::{
    hw::{ps2::ctrl::PS2Ctrl, qemu::ramfb::RamFb, uart::LpcUart},
    vmm::Machine,
};
use propolis_api_types::{
    instance_spec::v0::InstanceSpecV0, InstanceProperties,
    InstanceStateMonitorResponse,
};

use crate::serial::Serial;

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

#[derive(Debug, thiserror::Error)]
pub(crate) enum VmError {
    #[error("VM already initialized")]
    AlreadyInitialized,

    #[error("VM initialization failed")]
    InitializationFailed(#[source] anyhow::Error),
}

/// The top-level VM wrapper type. Callers are expected to wrap this in an
/// `Arc`.
pub(crate) struct Vm {
    /// A reference to the VM state machine.
    state: RwLock<VmState>,
}

struct VmObjects {
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

    pub(crate) fn device_by_name(
        &self,
        name: &String,
    ) -> Option<Arc<dyn propolis::common::Lifecycle>> {
        self.vm_objects.devices.get(name).cloned()
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

    objects: tokio::sync::RwLock<VmObjects>,
    services: tokio::sync::Mutex<Option<services::VmServices>>,
}

impl ActiveVm {
    pub(crate) fn log(&self) -> &slog::Logger {
        &self.log
    }

    pub(crate) async fn objects(
        &self,
    ) -> tokio::sync::RwLockReadGuard<'_, VmObjects> {
        self.objects.read().await
    }

    async fn stop_services(&self) {
        let services = self.services.lock().await.take().unwrap();
        services.stop(&self.log).await;
    }
}

struct DefunctVm {
    external_state_rx: InstanceStateRx,
    properties: InstanceProperties,
    spec: InstanceSpecV0,
}

#[allow(clippy::large_enum_variant)]
enum VmState {
    NoVm,
    WaitingToStart,
    Active(Weak<ActiveVm>),
    Defunct(DefunctVm),
}

pub(super) struct EnsureOptions {
    pub toml_config: Arc<crate::server::VmTomlConfig>,
    pub use_reservoir: bool,
    pub oximeter_registry: Option<ProducerRegistry>,
    pub nexus_client: Option<nexus_client::Client>,
}

impl Vm {
    pub fn new() -> Arc<Self> {
        Arc::new(Self { state: RwLock::new(VmState::NoVm) })
    }

    fn vm_state(&self) -> RwLockReadGuard<'_, VmState> {
        self.state.read().unwrap()
    }

    pub(super) fn active_vm(&self) -> Option<Arc<ActiveVm>> {
        let guard = self.vm_state();
        if let VmState::Active(weak) = &*guard {
            weak.upgrade()
        } else {
            None
        }
    }

    fn start_failed(&self) {
        let mut guard = self.state.write().unwrap();
        match *guard {
            VmState::WaitingToStart => *guard = VmState::NoVm,
            _ => unreachable!(
                "only a starting VM's state worker calls start_failed"
            ),
        }
    }

    fn make_active(&self, active: Arc<ActiveVm>) {
        let mut guard = self.state.write().unwrap();
        let old = std::mem::replace(&mut *guard, VmState::NoVm);
        match old {
            VmState::WaitingToStart => {
                *guard = VmState::Active(Arc::downgrade(&active))
            }
            _ => unreachable!(
                "only a starting VM's state worker calls make_active"
            ),
        }
    }

    async fn make_defunct(&self) {
        let mut guard = self.state.write().unwrap();
        let old = std::mem::replace(&mut *guard, VmState::NoVm);
        match old {
            VmState::Active(vm) => {
                let active = vm.upgrade().expect("state driver holds a ref");
                *guard = VmState::Defunct(DefunctVm {
                    external_state_rx: active.external_state_rx.clone(),
                    properties: active.properties.clone(),
                    spec: active.objects().await.instance_spec.clone(),
                });
            }
            _ => unreachable!(
                "only an active VM's state worker calls make_defunct"
            ),
        }
    }

    pub async fn ensure(
        self: &Arc<Self>,
        log: slog::Logger,
        ensure_request: propolis_api_types::InstanceSpecEnsureRequest,
        options: EnsureOptions,
    ) -> anyhow::Result<propolis_api_types::InstanceEnsureResponse, VmError>
    {
        // Take the lock for writing, since in the common case this call will be
        // creating a new VM and there's no easy way to upgrade from a reader
        // lock to a writer lock.
        let mut guard = self.state.write().unwrap();

        if matches!(*guard, VmState::WaitingToStart | VmState::Active(_)) {
            return Err(VmError::AlreadyInitialized);
        }

        *guard = VmState::WaitingToStart;

        let state_driver = state_driver::StateDriver::new(
            log,
            self.clone(),
            Arc::new(input_queue),
            external_tx,
        );

        let (ensure_reply_tx, ensure_rx) = tokio::sync::oneshot::channel();

        tokio::spawn(async move {
            state_driver
                .run(ensure_request, ensure_reply_tx, options, external_rx)
                .await
        });

        ensure_rx.await
    }
}
