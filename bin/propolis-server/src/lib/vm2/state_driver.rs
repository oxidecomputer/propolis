// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! It drives the state vroom vroom

use std::{
    sync::{Arc, Condvar, Mutex},
    time::Duration,
};

use propolis_api_types::{
    instance_spec::VersionedInstanceSpec, InstanceProperties, InstanceState,
};
use slog::info;
use uuid::Uuid;

use crate::{
    initializer::{
        build_instance, MachineInitializer, MachineInitializerState,
    },
    migrate::MigrateRole,
    vcpu_tasks::{VcpuTaskController, VcpuTasks},
};

use super::{
    guest_event::{self, GuestEvent},
    lifecycle_ops,
};

struct MigrationStateUpdate {
    state: propolis_api_types::MigrationState,
    id: Uuid,
    role: MigrateRole,
}

impl MigrationStateUpdate {
    fn apply_to(
        self,
        old: propolis_api_types::InstanceMigrateStatusResponse,
    ) -> propolis_api_types::InstanceMigrateStatusResponse {
        let new = propolis_api_types::InstanceMigrationStatus {
            id: self.id,
            state: self.state,
        };
        match self.role {
            MigrateRole::Destination => {
                propolis_api_types::InstanceMigrateStatusResponse {
                    migration_in: Some(new),
                    migration_out: old.migration_out,
                }
            }
            MigrateRole::Source => {
                propolis_api_types::InstanceMigrateStatusResponse {
                    migration_in: old.migration_in,
                    migration_out: Some(new),
                }
            }
        }
    }
}

enum ExternalStateUpdate {
    Instance(InstanceState),
    Migration(MigrationStateUpdate),
    Complete(InstanceState, MigrationStateUpdate),
}

#[derive(Debug, PartialEq, Eq)]
enum HandleEventOutcome {
    Continue,
    Exit,
}

#[derive(Debug)]
enum InputQueueEvent {
    ExternalRequest(super::request_queue::ExternalRequest),
    GuestEvent(GuestEvent),
}

struct InputQueueInner {
    external_requests: super::request_queue::ExternalRequestQueue,
    guest_events: super::guest_event::GuestEventQueue,
}

impl InputQueueInner {
    fn new(log: slog::Logger) -> Self {
        Self {
            external_requests: super::request_queue::ExternalRequestQueue::new(
                log,
            ),
            guest_events: super::guest_event::GuestEventQueue::default(),
        }
    }
}

pub(super) struct InputQueue {
    inner: Mutex<InputQueueInner>,
    cv: Condvar,
}

impl InputQueue {
    pub(super) fn new(log: slog::Logger) -> Self {
        Self {
            inner: Mutex::new(InputQueueInner::new(log)),
            cv: Condvar::new(),
        }
    }

    fn wait_for_next_event(&self) -> InputQueueEvent {
        let guard = self.inner.lock().unwrap();
        let mut guard = self
            .cv
            .wait_while(guard, |i| {
                i.external_requests.is_empty() && i.guest_events.is_empty()
            })
            .unwrap();

        if let Some(guest_event) = guard.guest_events.pop_front() {
            InputQueueEvent::GuestEvent(guest_event)
        } else {
            InputQueueEvent::ExternalRequest(
                guard.external_requests.pop_front().unwrap(),
            )
        }
    }

    fn notify_instance_state_change(
        &self,
        state: super::request_queue::InstanceStateChange,
    ) {
        let guard = self.inner.lock().unwrap();
        guard.external_requests.notify_instance_state_change(state);
    }
}

impl guest_event::GuestEventHandler for InputQueue {
    fn suspend_halt_event(&self, when: Duration) {
        let mut guard = self.inner.lock().unwrap();
        if guard
            .guest_events
            .enqueue(guest_event::GuestEvent::VcpuSuspendHalt(when))
        {
            self.cv.notify_all();
        }
    }

    fn suspend_reset_event(&self, when: Duration) {
        let mut guard = self.inner.lock().unwrap();
        if guard
            .guest_events
            .enqueue(guest_event::GuestEvent::VcpuSuspendReset(when))
        {
            self.cv.notify_all();
        }
    }

    fn suspend_triple_fault_event(&self, vcpu_id: i32, when: Duration) {
        let mut guard = self.inner.lock().unwrap();
        if guard.guest_events.enqueue(
            guest_event::GuestEvent::VcpuSuspendTripleFault(vcpu_id, when),
        ) {
            self.cv.notify_all();
        }
    }

    fn unhandled_vm_exit(
        &self,
        vcpu_id: i32,
        exit: propolis::exits::VmExitKind,
    ) {
        panic!("vCPU {}: Unhandled VM exit: {:?}", vcpu_id, exit);
    }

    fn io_error_event(&self, vcpu_id: i32, error: std::io::Error) {
        panic!("vCPU {}: Unhandled vCPU error: {}", vcpu_id, error);
    }
}

impl guest_event::ChipsetEventHandler for InputQueue {
    fn chipset_halt(&self) {
        let mut guard = self.inner.lock().unwrap();
        if guard.guest_events.enqueue(guest_event::GuestEvent::ChipsetHalt) {
            self.cv.notify_all();
        }
    }

    fn chipset_reset(&self) {
        let mut guard = self.inner.lock().unwrap();
        if guard.guest_events.enqueue(guest_event::GuestEvent::ChipsetReset) {
            self.cv.notify_all();
        }
    }
}

/// The context for a VM state driver task.
pub(super) struct StateDriver {
    log: slog::Logger,
    parent_vm: Arc<super::Vm>,
    input_queue: Arc<InputQueue>,
    external_state_tx: super::InstanceStateTx,
    paused: bool,
    vcpu_tasks: Option<Box<dyn VcpuTaskController>>,
    vm_lifecycle: Option<Arc<dyn lifecycle_ops::VmLifecycle>>,
    migration_src_state: crate::migrate::source::PersistentState,
}

impl StateDriver {
    pub(super) fn new(
        log: slog::Logger,
        vm: Arc<super::Vm>,
        input_queue: Arc<InputQueue>,
        external_state_tx: super::InstanceStateTx,
    ) -> Self {
        let log = log.new(slog::o!("component" => "state_driver"));
        Self {
            log,
            parent_vm: vm,
            input_queue,
            external_state_tx,
            paused: false,
            vcpu_tasks: None,
            vm_lifecycle: None,
            migration_src_state: Default::default(),
        }
    }

    pub(super) async fn run(
        mut self,
        ensure_request: propolis_api_types::InstanceSpecEnsureRequest,
        ensure_options: super::EnsureOptions,
        external_state_rx: super::InstanceStateRx,
    ) {
        if let Ok(active) = self
            .initialize_vm(ensure_request, ensure_options, external_state_rx)
            .await
        {
            self.parent_vm.make_active(active.clone());
            self.vm_lifecycle =
                Some(active as Arc<dyn lifecycle_ops::VmLifecycle>);
        } else {
            // TODO(gjc) also publish that it failed. we're the only thing that
            // has the external tx so need to do that from here
            self.parent_vm.start_failed();
            return;
        }

        self.run_loop().await;
    }

    fn update_external_state(&mut self, state: ExternalStateUpdate) {
        let (instance_state, migration_state) = match state {
            ExternalStateUpdate::Instance(i) => (Some(i), None),
            ExternalStateUpdate::Migration(m) => (None, Some(m)),
            ExternalStateUpdate::Complete(i, m) => (Some(i), Some(m)),
        };

        let propolis_api_types::InstanceStateMonitorResponse {
            state: old_instance,
            migration: old_migration,
            gen: old_gen,
        } = self.external_state_tx.borrow().clone();

        let state = instance_state.unwrap_or(old_instance);
        let migration = if let Some(migration_state) = migration_state {
            migration_state.apply_to(old_migration)
        } else {
            old_migration
        };

        let gen = old_gen + 1;
        info!(self.log, "publishing new instance state";
              "gen" => gen,
              "state" => ?state,
              "migration" => ?migration);

        let _ = self.external_state_tx.send(
            propolis_api_types::InstanceStateMonitorResponse {
                gen,
                state,
                migration,
            },
        );
    }

    async fn initialize_vm(
        &mut self,
        request: propolis_api_types::InstanceSpecEnsureRequest,
        options: super::EnsureOptions,
        external_state_rx: super::InstanceStateRx,
    ) -> anyhow::Result<Arc<super::ActiveVm>> {
        let active_vm = match request.migrate {
            None => {
                let vm_objects = self
                    .initialize_vm_from_spec(
                        &request.properties,
                        &request.instance_spec,
                        options,
                    )
                    .await?;
                let VersionedInstanceSpec::V0(v0_spec) = request.instance_spec;
                let active_vm = Arc::new(super::ActiveVm {
                    parent: self.parent_vm.clone(),
                    state_driver_queue: self.input_queue.clone(),
                    external_state_rx,
                    properties: request.properties,
                    spec: v0_spec,
                    objects: vm_objects,
                });

                active_vm
            }
            Some(_migrate_request) => todo!("gjc"),
        };

        Ok(active_vm)
    }

    /// Initializes all of the components of a VM from the supplied
    /// specification.
    async fn initialize_vm_from_spec(
        &mut self,
        properties: &InstanceProperties,
        spec: &VersionedInstanceSpec,
        options: super::EnsureOptions,
    ) -> anyhow::Result<super::VmObjects> {
        info!(self.log, "initializing new VM";
              "spec" => #?spec,
              "properties" => #?properties,
              "use_reservoir" => options.use_reservoir,
              "bootrom" => %options.toml_config.bootrom.display());

        let vmm_log = self.log.new(slog::o!("component" => "vmm"));

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
            log: self.log.clone(),
            machine: &machine,
            devices: Default::default(),
            block_backends: Default::default(),
            crucible_backends: Default::default(),
            spec: &v0_spec,
            properties: &properties,
            toml_config: &options.toml_config,
            producer_registry: options.oximeter_registry,
            state: MachineInitializerState::default(),
        };

        init.initialize_rom(options.toml_config.bootrom.as_path())?;
        let chipset = init.initialize_chipset(
            &(self.input_queue.clone()
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
        info!(
            self.log,
            "`omicron-build` feature enabled, ignoring any test devices"
        );

        #[cfg(feature = "falcon")]
        init.initialize_softnpu_ports(&chipset)?;
        #[cfg(feature = "falcon")]
        init.initialize_9pfs(&chipset)?;

        init.initialize_storage_devices(&chipset, options.nexus_client)?;
        let ramfb = init.initialize_fwcfg(v0_spec.devices.board.cpus)?;
        init.initialize_cpus()?;
        let vcpu_tasks = crate::vcpu_tasks::VcpuTasks::new(
            &machine,
            &(self.input_queue.clone()
                as Arc<dyn super::guest_event::GuestEventHandler>),
            self.log.new(slog::o!("component" => "vcpu_tasks")),
        )?;

        let MachineInitializer {
            devices,
            block_backends,
            crucible_backends,
            ..
        } = init;

        self.vcpu_tasks = Some(vcpu_tasks);
        Ok(super::VmObjects {
            machine,
            lifecycle_components: devices,
            block_backends,
            crucible_backends,
            com1,
            framebuffer: Some(ramfb),
            ps2ctrl,
        })
    }

    async fn run_loop(mut self) {
        info!(self.log, "state driver launched");

        loop {
            let event = self.input_queue.wait_for_next_event();
            info!(self.log, "state driver handling event"; "event" => ?event);

            let outcome = match event {
                InputQueueEvent::ExternalRequest(req) => {
                    self.handle_external_request(req).await
                }
                InputQueueEvent::GuestEvent(event) => {
                    self.handle_guest_event(event).await
                }
            };

            let outcome = self.handle_event(event).await;
            info!(self.log, "state driver handled event"; "outcome" => ?outcome);
            if outcome == HandleEventOutcome::Exit {
                break;
            }
        }

        info!(self.log, "state driver exiting");
    }

    async fn handle_guest_event(
        &mut self,
        event: GuestEvent,
    ) -> HandleEventOutcome {
        match event {
            GuestEvent::VcpuSuspendHalt(_when) => {
                info!(self.log, "Halting due to VM suspend event",);
                self.do_halt();
                HandleEventOutcome::Exit
            }
            GuestEvent::VcpuSuspendReset(_when) => {
                info!(self.log, "Resetting due to VM suspend event");
                self.do_reboot().await;
                HandleEventOutcome::Continue
            }
            GuestEvent::VcpuSuspendTripleFault(vcpu_id, _when) => {
                info!(
                    self.log,
                    "Resetting due to triple fault on vCPU {}", vcpu_id
                );
                self.do_reboot().await;
                HandleEventOutcome::Continue
            }
            GuestEvent::ChipsetHalt => {
                info!(self.log, "Halting due to chipset-driven halt");
                self.do_halt();
                HandleEventOutcome::Exit
            }
            GuestEvent::ChipsetReset => {
                info!(self.log, "Resetting due to chipset-driven reset");
                self.do_reboot().await;
                HandleEventOutcome::Continue
            }
        }
    }

    fn handle_external_request(
        &mut self,
        request: super::request_queue::ExternalRequest,
    ) -> HandleEventOutcome {
        todo!("gjc");
    }

    async fn do_reboot(&mut self) {
        info!(self.log, "resetting instance");

        self.update_external_state(ExternalStateUpdate::Instance(
            InstanceState::Rebooting,
        ));

        // Reboot is implemented as a pause -> reset -> resume transition.
        //
        // First, pause the vCPUs and all devices so no partially-completed
        // work is present.
        self.vcpu_tasks().pause_all();
        self.vm_lifecycle().pause_devices().await;

        // Reset all entities and the VM's bhyve state, then reset the vCPUs.
        // The vCPU reset must come after the bhyve reset.
        self.vm_lifecycle().reset_devices_and_machine();
        self.reset_vcpus();

        // Resume devices so they're ready to do more work, then resume vCPUs.
        self.vm_lifecycle().resume_devices();
        self.vcpu_tasks().resume_all();

        // Notify other consumers that the instance successfully rebooted and is
        // now back to Running.
        self.input_queue.notify_instance_state_change(
            super::request_queue::InstanceStateChange::Rebooted,
        );
        self.update_external_state(ExternalStateUpdate::Instance(
            InstanceState::Running,
        ));
    }

    fn reset_vcpus(&mut self) {
        self.vcpu_tasks().new_generation();
        self.vm_lifecycle.as_ref().unwrap().reset_vcpu_state();
    }

    fn vcpu_tasks(&mut self) -> &mut dyn VcpuTaskController {
        self.vcpu_tasks.as_mut().unwrap().as_mut()
    }

    fn vm_lifecycle(&self) -> &dyn lifecycle_ops::VmLifecycle {
        self.vm_lifecycle.as_ref().unwrap().as_ref()
    }
}
