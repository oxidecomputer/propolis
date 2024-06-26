// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! It drives the state vroom vroom

use std::{
    sync::{Arc, Condvar, Mutex},
    time::Duration,
};

use propolis_api_types::{
    instance_spec::{
        components::backends::CrucibleStorageBackend, v0::StorageBackendV0,
        VersionedInstanceSpec,
    },
    InstanceMigrateInitiateResponse, InstanceProperties, InstanceState,
    MigrationState,
};
use slog::{error, info};
use uuid::Uuid;

use crate::{
    initializer::{
        build_instance, MachineInitializer, MachineInitializerState,
    },
    migrate::MigrateRole,
    vcpu_tasks::VcpuTaskController,
    vm::{
        migrate_commands::MigrateTargetCommand,
        state_publisher::ExternalStateUpdate,
    },
};

use super::{
    guest_event::{self, GuestEvent},
    migrate_commands::{
        MigrateSourceCommand, MigrateSourceResponse, MigrateTargetResponse,
        MigrateTaskEvent,
    },
    request_queue::ExternalRequest,
    state_publisher::{MigrationStateUpdate, StatePublisher},
    VmError, VmObjects,
};

#[derive(Debug, PartialEq, Eq)]
enum HandleEventOutcome {
    Continue,
    Exit,
}

/// A reason for starting a VM.
#[derive(Debug, PartialEq, Eq)]
enum VmStartReason {
    MigratedIn,
    ExplicitRequest,
}

#[derive(Debug)]
enum InputQueueEvent {
    ExternalRequest(ExternalRequest),
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
        tokio::task::block_in_place(|| {
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
        })
    }

    fn notify_instance_state_change(
        &self,
        state: super::request_queue::InstanceStateChange,
    ) {
        let mut guard = self.inner.lock().unwrap();
        guard.external_requests.notify_instance_state_change(state);
    }

    pub(super) fn queue_external_request(
        &self,
        request: ExternalRequest,
    ) -> Result<(), super::request_queue::RequestDeniedReason> {
        let mut inner = self.inner.lock().unwrap();
        let result = inner.external_requests.try_queue(request);
        if result.is_ok() {
            self.cv.notify_one();
        }
        result
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
struct StateDriver {
    log: slog::Logger,
    parent: Arc<super::Vm>,
    active_vm: Arc<super::ActiveVm>,
    input_queue: Arc<InputQueue>,
    external_state: StatePublisher,
    paused: bool,
    vcpu_tasks: Box<dyn VcpuTaskController>,
    migration_src_state: crate::migrate::source::PersistentState,
}

pub(super) async fn run_state_driver(
    log: slog::Logger,
    vm: Arc<super::Vm>,
    mut external_publisher: StatePublisher,
    ensure_request: propolis_api_types::InstanceSpecEnsureRequest,
    ensure_result_tx: tokio::sync::oneshot::Sender<
        Result<propolis_api_types::InstanceEnsureResponse, VmError>,
    >,
    ensure_options: super::EnsureOptions,
) -> StatePublisher {
    let input_queue = Arc::new(InputQueue::new(
        log.new(slog::o!("component" => "request_queue")),
    ));

    let migration_in_id =
        ensure_request.migrate.as_ref().map(|req| req.migration_id);
    let (vm_objects, vcpu_tasks) = match match ensure_request.migrate {
        None => {
            initialize_vm_from_spec(
                &log,
                &input_queue,
                &ensure_request.properties,
                &ensure_request.instance_spec,
                &ensure_options,
            )
            .await
        }
        Some(migrate_request) => {
            migrate_as_target(
                &log,
                &input_queue,
                &ensure_request.properties,
                &ensure_request.instance_spec,
                &ensure_options,
                migrate_request,
                &mut external_publisher,
            )
            .await
        }
    } {
        Ok(objects) => objects,
        Err(e) => {
            external_publisher
                .update(ExternalStateUpdate::Instance(InstanceState::Failed));
            vm.start_failed();
            let _ =
                ensure_result_tx.send(Err(VmError::InitializationFailed(e)));
            return external_publisher;
        }
    };

    let services = super::services::VmServices::new(
        &log,
        &vm,
        &vm_objects,
        &ensure_request.properties,
        &ensure_options,
    )
    .await;

    // All the VM components now exist, so allow external callers to
    // interact with the VM.
    //
    // Order matters here: once the ensure result is sent, an external
    // caller needs to observe that an active VM is present.
    let active_vm =
        vm.make_active(&log, input_queue.clone(), vm_objects, services);

    let _ =
        ensure_result_tx.send(Ok(propolis_api_types::InstanceEnsureResponse {
            migrate: migration_in_id
                .map(|id| InstanceMigrateInitiateResponse { migration_id: id }),
        }));

    let state_driver = StateDriver {
        log,
        parent: vm.clone(),
        active_vm,
        input_queue,
        external_state: external_publisher,
        paused: false,
        vcpu_tasks,
        migration_src_state: Default::default(),
    };

    state_driver.run(migration_in_id.is_some()).await
}

impl StateDriver {
    pub(super) async fn run(mut self, migrated_in: bool) -> StatePublisher {
        info!(self.log, "state driver launched");

        if migrated_in {
            if self.start_vm(VmStartReason::MigratedIn).await.is_ok() {
                self.run_loop().await;
            }
        } else {
            self.run_loop().await;
        }

        self.parent.set_rundown().await;
        self.external_state
    }

    async fn run_loop(&mut self) {
        info!(self.log, "state driver entered main loop");
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

            info!(self.log, "state driver handled event"; "outcome" => ?outcome);
            if outcome == HandleEventOutcome::Exit {
                break;
            }
        }

        info!(self.log, "state driver exiting");
    }

    async fn start_vm(
        &mut self,
        start_reason: VmStartReason,
    ) -> anyhow::Result<()> {
        info!(self.log, "starting instance"; "reason" => ?start_reason);

        let start_result = {
            let (vm_objects, vcpu_tasks) = self.vm_objects_and_cpus().await;
            match start_reason {
                VmStartReason::ExplicitRequest => {
                    reset_vcpus(&vm_objects, vcpu_tasks);
                }
                VmStartReason::MigratedIn => {
                    vm_objects.resume_vm();
                }
            }

            let result = vm_objects.start_devices().await;
            if result.is_ok() {
                vcpu_tasks.resume_all();
            }

            result
        };

        match &start_result {
            Ok(()) => self.publish_steady_state(InstanceState::Running),
            Err(e) => {
                error!(&self.log, "failed to start devices";
                                 "error" => ?e);
                self.publish_steady_state(InstanceState::Failed);
            }
        }

        start_result
    }

    async fn handle_guest_event(
        &mut self,
        event: GuestEvent,
    ) -> HandleEventOutcome {
        match event {
            GuestEvent::VcpuSuspendHalt(_when) => {
                info!(self.log, "Halting due to VM suspend event",);
                self.do_halt().await;
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
                self.do_halt().await;
                HandleEventOutcome::Exit
            }
            GuestEvent::ChipsetReset => {
                info!(self.log, "Resetting due to chipset-driven reset");
                self.do_reboot().await;
                HandleEventOutcome::Continue
            }
        }
    }

    async fn handle_external_request(
        &mut self,
        request: ExternalRequest,
    ) -> HandleEventOutcome {
        match request {
            ExternalRequest::Start => {
                match self.start_vm(VmStartReason::ExplicitRequest).await {
                    Ok(_) => HandleEventOutcome::Continue,
                    Err(_) => HandleEventOutcome::Exit,
                }
            }
            ExternalRequest::MigrateAsSource { migration_id, websock } => {
                self.migrate_as_source(migration_id, websock.into_inner())
                    .await;

                // The callee either queues its own stop request (on a
                // successful migration out) or resumes the VM (on a failed
                // migration out). Either way, the main loop can just proceed to
                // process the queue as normal.
                HandleEventOutcome::Continue
            }
            ExternalRequest::Reboot => {
                self.do_reboot().await;
                HandleEventOutcome::Continue
            }
            ExternalRequest::Stop => {
                self.do_halt().await;
                HandleEventOutcome::Exit
            }
            ExternalRequest::ReconfigureCrucibleVolume {
                disk_name,
                backend_id,
                new_vcr_json,
                result_tx,
            } => {
                let _ = result_tx.send(
                    self.reconfigure_crucible_volume(
                        disk_name,
                        &backend_id,
                        new_vcr_json,
                    )
                    .await,
                );
                HandleEventOutcome::Continue
            }
        }
    }

    async fn do_reboot(&mut self) {
        info!(self.log, "resetting instance");

        self.external_state
            .update(ExternalStateUpdate::Instance(InstanceState::Rebooting));

        {
            let (vm_objects, vcpu_tasks) = self.vm_objects_and_cpus().await;

            // Reboot is implemented as a pause -> reset -> resume transition.
            //
            // First, pause the vCPUs and all devices so no partially-completed
            // work is present.
            vcpu_tasks.pause_all();
            vm_objects.pause_devices().await;

            // Reset all entities and the VM's bhyve state, then reset the
            // vCPUs. The vCPU reset must come after the bhyve reset.
            vm_objects.reset_devices_and_machine();
            reset_vcpus(&vm_objects, vcpu_tasks);

            // Resume devices so they're ready to do more work, then resume
            // vCPUs.
            vm_objects.resume_devices();
            vcpu_tasks.resume_all();
        }

        // Notify other consumers that the instance successfully rebooted and is
        // now back to Running.
        self.input_queue.notify_instance_state_change(
            super::request_queue::InstanceStateChange::Rebooted,
        );
        self.external_state
            .update(ExternalStateUpdate::Instance(InstanceState::Running));
    }

    async fn do_halt(&mut self) {
        info!(self.log, "stopping instance");
        self.external_state
            .update(ExternalStateUpdate::Instance(InstanceState::Stopping));

        // Entities expect to be paused before being halted. Note that the VM
        // may be paused already if it is being torn down after a successful
        // migration out.
        if !self.paused {
            self.pause().await;
        }

        self.vcpu_tasks.exit_all();
        self.vm_objects().await.halt_devices().await;
        self.publish_steady_state(InstanceState::Stopped);
    }

    async fn pause(&mut self) {
        assert!(!self.paused);
        self.vcpu_tasks.pause_all();
        {
            let objects = self.vm_objects().await;
            objects.pause_devices().await;
            objects.pause_vm();
        }
        self.paused = true;
    }

    async fn resume(&mut self) {
        assert!(self.paused);
        {
            let objects = self.vm_objects().await;
            objects.resume_vm();
            objects.resume_devices();
        }
        self.vcpu_tasks.resume_all();
        self.paused = false;
    }

    fn publish_steady_state(&mut self, state: InstanceState) {
        let change = match state {
            InstanceState::Running => {
                super::request_queue::InstanceStateChange::StartedRunning
            }
            InstanceState::Stopped => {
                super::request_queue::InstanceStateChange::Stopped
            }
            InstanceState::Failed => {
                super::request_queue::InstanceStateChange::Failed
            }
            _ => panic!(
                "Called publish_steady_state on non-terminal state {:?}",
                state
            ),
        };

        self.input_queue.notify_instance_state_change(change);
        self.external_state.update(ExternalStateUpdate::Instance(state));
    }

    async fn vm_objects(&self) -> tokio::sync::RwLockReadGuard<'_, VmObjects> {
        self.active_vm.objects().await
    }

    async fn vm_objects_mut(
        &self,
    ) -> tokio::sync::RwLockWriteGuard<'_, VmObjects> {
        self.active_vm.objects_mut().await
    }

    async fn vm_objects_and_cpus(
        &mut self,
    ) -> (
        tokio::sync::RwLockReadGuard<'_, VmObjects>,
        &mut dyn VcpuTaskController,
    ) {
        (self.active_vm.objects().await, self.vcpu_tasks.as_mut())
    }

    async fn migrate_as_source(
        &mut self,
        migration_id: Uuid,
        websock: dropshot::WebsocketConnection,
    ) {
        let conn = tokio_tungstenite::WebSocketStream::from_raw_socket(
            websock.into_inner(),
            tokio_tungstenite::tungstenite::protocol::Role::Server,
            None,
        )
        .await;

        // Negotiate the migration protocol version with the target.
        let Ok(migrate_ctx) =
            crate::migrate::source_start(&self.log, migration_id, conn).await
        else {
            return;
        };

        // Publish that migration is in progress before actually launching the
        // migration task.
        self.external_state.update(ExternalStateUpdate::Complete(
            InstanceState::Migrating,
            MigrationStateUpdate {
                state: MigrationState::Sync,
                id: migration_id,
                role: MigrateRole::Source,
            },
        ));

        let (command_tx, mut command_rx) = tokio::sync::mpsc::channel(1);
        let (response_tx, response_rx) = tokio::sync::mpsc::channel(1);
        let vm_for_task = self.active_vm.clone();
        let mut migrate_task = tokio::spawn(async move {
            crate::migrate::source::migrate(
                vm_for_task,
                command_tx,
                response_rx,
                migrate_ctx.conn,
                migrate_ctx.protocol,
            )
            .await
        });

        loop {
            match next_migrate_task_event(
                &mut migrate_task,
                &mut command_rx,
                &self.log,
            )
            .await
            {
                MigrateTaskEvent::TaskExited(res) => {
                    if res.is_ok() {
                        self.active_vm
                            .state_driver_queue
                            .queue_external_request(ExternalRequest::Stop)
                            .expect("can always queue a request to stop");
                    } else {
                        if self.paused {
                            self.resume().await;
                        }

                        self.publish_steady_state(InstanceState::Running);
                    }

                    return;
                }

                // N.B. When handling a command that requires a reply, do not
                //      return early if the reply fails to send. Instead,
                //      loop back around and let the `TaskExited` path restore
                //      the VM to the correct state.
                MigrateTaskEvent::Command(cmd) => match cmd {
                    MigrateSourceCommand::UpdateState(state) => {
                        self.external_state.update(
                            ExternalStateUpdate::Migration(
                                MigrationStateUpdate {
                                    id: migration_id,
                                    state,
                                    role: MigrateRole::Source,
                                },
                            ),
                        );
                    }
                    MigrateSourceCommand::Pause => {
                        self.pause().await;
                        let _ = response_tx
                            .send(MigrateSourceResponse::Pause(Ok(())))
                            .await;
                    }
                    MigrateSourceCommand::QueryRedirtyingFailed => {
                        let has_failed =
                            self.migration_src_state.has_redirtying_ever_failed;
                        let _ = response_tx
                            .send(MigrateSourceResponse::RedirtyingFailed(
                                has_failed,
                            ))
                            .await;
                    }
                    MigrateSourceCommand::RedirtyingFailed => {
                        self.migration_src_state.has_redirtying_ever_failed =
                            true;
                    }
                },
            }
        }
    }

    async fn reconfigure_crucible_volume(
        &self,
        disk_name: String,
        backend_id: &Uuid,
        new_vcr_json: String,
    ) -> super::CrucibleReplaceResult {
        info!(self.log, "request to replace Crucible VCR";
              "disk_name" => %disk_name,
              "backend_id" => %backend_id);

        let mut objects = self.vm_objects_mut().await;

        fn spec_element_not_found(disk_name: &str) -> dropshot::HttpError {
            let msg = format!("Crucible backend for {:?} not found", disk_name);
            dropshot::HttpError::for_not_found(Some(msg.clone()), msg)
        }

        let (readonly, old_vcr_json) = {
            let StorageBackendV0::Crucible(bes) = objects
                .instance_spec
                .backends
                .storage_backends
                .get(&disk_name)
                .ok_or_else(|| spec_element_not_found(&disk_name))?
            else {
                return Err(spec_element_not_found(&disk_name));
            };

            (bes.readonly, &bes.request_json)
        };

        let replace_result = {
            let backend =
                objects.crucible_backends.get(backend_id).ok_or_else(|| {
                    let msg =
                        format!("No crucible backend for id {backend_id}");
                    dropshot::HttpError::for_not_found(Some(msg.clone()), msg)
                })?;

            backend.vcr_replace(old_vcr_json, &new_vcr_json).await.map_err(
                |e| {
                    dropshot::HttpError::for_bad_request(
                        Some(e.to_string()),
                        e.to_string(),
                    )
                },
            )
        }?;

        let new_bes = StorageBackendV0::Crucible(CrucibleStorageBackend {
            readonly,
            request_json: new_vcr_json,
        });

        objects
            .instance_spec
            .backends
            .storage_backends
            .insert(disk_name, new_bes);

        info!(self.log, "replaced Crucible VCR"; "backend_id" => %backend_id);

        Ok(replace_result)
    }
}

fn reset_vcpus(
    vm_objects: &VmObjects,
    vcpu_tasks: &mut dyn VcpuTaskController,
) {
    vcpu_tasks.new_generation();
    vm_objects.reset_vcpu_state();
}

async fn initialize_vm_from_spec(
    log: &slog::Logger,
    event_queue: &Arc<InputQueue>,
    properties: &InstanceProperties,
    spec: &VersionedInstanceSpec,
    options: &super::EnsureOptions,
) -> anyhow::Result<(VmObjects, Box<dyn VcpuTaskController>)> {
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
        event_queue.clone() as Arc<dyn super::guest_event::GuestEventHandler>,
        log.new(slog::o!("component" => "vcpu_tasks")),
    )?);

    let MachineInitializer {
        devices, block_backends, crucible_backends, ..
    } = init;

    Ok((
        VmObjects {
            log: log.clone(),
            instance_spec: v0_spec.clone(),
            machine,
            lifecycle_components: devices,
            block_backends,
            crucible_backends,
            com1,
            framebuffer: Some(ramfb),
            ps2ctrl,
        },
        vcpu_tasks as Box<dyn VcpuTaskController>,
    ))
}

async fn migrate_as_target(
    log: &slog::Logger,
    event_queue: &Arc<InputQueue>,
    properties: &InstanceProperties,
    spec: &VersionedInstanceSpec,
    options: &super::EnsureOptions,
    api_request: propolis_api_types::InstanceMigrateInitiateRequest,
    external_state: &mut StatePublisher,
) -> anyhow::Result<(VmObjects, Box<dyn VcpuTaskController>)> {
    // Use the information in the supplied migration request to connect to the
    // migration source and negotiate the protocol verison to use.
    let migrate_ctx = crate::migrate::dest_initiate(
        log,
        api_request,
        options.local_server_addr,
    )
    .await?;

    // Spin up a task to run the migration protocol proper. To avoid sending the
    // entire VM context over to the migration task, create command and response
    // channels to allow the migration task to delegate work back to this
    // routine.
    let log_for_task = log.clone();
    let (command_tx, mut command_rx) = tokio::sync::mpsc::channel(1);
    let (response_tx, response_rx) = tokio::sync::mpsc::channel(1);
    let mut migrate_task = tokio::spawn(async move {
        crate::migrate::destination::migrate(
            &log_for_task,
            command_tx,
            response_rx,
            migrate_ctx.conn,
            migrate_ctx.local_addr,
            migrate_ctx.protocol,
        )
        .await
    });

    async fn init_sequence(
        log: &slog::Logger,
        event_queue: &Arc<InputQueue>,
        properties: &InstanceProperties,
        spec: &VersionedInstanceSpec,
        options: &super::EnsureOptions,
        command_rx: &mut tokio::sync::mpsc::Receiver<MigrateTargetCommand>,
    ) -> anyhow::Result<(VmObjects, Box<dyn VcpuTaskController>)> {
        // Migration cannot proceed (in any protocol version) until the target
        // kernel VMM and Propolis components have been set up. The first
        // command from the migration task should be a request to set up these
        // components.
        let init_command = command_rx.recv().await.ok_or_else(|| {
            anyhow::anyhow!("migration task unexpectedly closed channel")
        })?;

        // TODO(#706) The only extant protocol version (V0 with RON encoding)
        // assumes that migration targets get an instance spec from the caller
        // of the `instance_ensure` API, that the target VM will be initialized
        // from this spec, and that device state will be imported in a later
        // migration phase. Another approach is to get an instance spec from the
        // source, amend it with information passed to the target, execute
        // enough of the migration protocol to get device state payloads, and
        // initialize everything in one fell swoop using the spec and payloads
        // as inputs.
        //
        // This requires a new protocol version, so for now, only look for a
        // request to initialize the VM from the caller-provided spec.
        let MigrateTargetCommand::InitializeFromExternalSpec = init_command
        else {
            error!(log, "migration protocol didn't init objects first";
               "first_cmd" => ?init_command);
            anyhow::bail!(
                "migration protocol didn't first ask to init objects"
            );
        };

        initialize_vm_from_spec(log, event_queue, properties, spec, options)
            .await
    }

    let (vm_objects, mut vcpu_tasks) = match init_sequence(
        log,
        event_queue,
        properties,
        spec,
        options,
        &mut command_rx,
    )
    .await
    {
        Ok(o) => o,
        Err(e) => {
            let _ = response_tx
                .send(MigrateTargetResponse::VmObjectsInitialized(Err(
                    e.to_string()
                )))
                .await;
            external_state.update(ExternalStateUpdate::Migration(
                MigrationStateUpdate {
                    id: migrate_ctx.migration_id,
                    state: MigrationState::Error,
                    role: MigrateRole::Source,
                },
            ));

            return Err(e);
        }
    };

    // The migration task imports device state by operating directly on the
    // newly-created VM objects. Before sending them to the task and allowing
    // migration to continue, prepare the VM's vCPUs and objects to have state
    // migrated into them.
    //
    // Ensure the VM's vCPUs are activated properly so that they can enter the
    // guest after migration. Do this before allowing the migration task to
    // continue so that reset doesn't overwrite any state written by migration.
    //
    // Pause the kernel VM so that emulated device state can be imported
    // consistently.
    reset_vcpus(&vm_objects, vcpu_tasks.as_mut());
    vm_objects.pause_vm();

    // Everything is ready, so send a reference to the newly-created VM to the
    // migration task. When the task exits, it drops this reference, allowing
    // this task to reclaim an owned `VmObjects` from the `Arc` wrapper.
    let vm_objects = Arc::new(vm_objects);
    if response_tx
        .send(MigrateTargetResponse::VmObjectsInitialized(Ok(
            vm_objects.clone()
        )))
        .await
        .is_err()
    {
        vm_objects.resume_vm();
        anyhow::bail!("migration task unexpectedly closed channel");
    }

    loop {
        let action =
            next_migrate_task_event(&mut migrate_task, &mut command_rx, log)
                .await;

        match action {
            MigrateTaskEvent::TaskExited(res) => match res {
                Ok(()) => {
                    let Ok(vm_objects) = Arc::try_unwrap(vm_objects) else {
                        panic!(
                            "migration task should have dropped its VM objects",
                        );
                    };

                    return Ok((vm_objects, vcpu_tasks));
                }
                Err(e) => {
                    error!(log, "target migration task failed";
                           "error" => %e);

                    vm_objects.resume_vm();
                    return Err(e.into());
                }
            },
            MigrateTaskEvent::Command(MigrateTargetCommand::UpdateState(
                state,
            )) => {
                external_state.update(ExternalStateUpdate::Migration(
                    MigrationStateUpdate {
                        state,
                        id: migrate_ctx.migration_id,
                        role: MigrateRole::Destination,
                    },
                ));
            }
            MigrateTaskEvent::Command(
                MigrateTargetCommand::InitializeFromExternalSpec,
            ) => {
                panic!("already received initialize-from-spec command");
            }
        }
    }
}

async fn next_migrate_task_event<E>(
    task: &mut tokio::task::JoinHandle<
        Result<(), crate::migrate::MigrateError>,
    >,
    command_rx: &mut tokio::sync::mpsc::Receiver<E>,
    log: &slog::Logger,
) -> MigrateTaskEvent<E> {
    if let Some(cmd) = command_rx.recv().await {
        return MigrateTaskEvent::Command(cmd);
    }

    // The sender side of the command channel is dropped, which means the
    // migration task is exiting. Wait for it to finish and snag its result.
    match task.await {
        Ok(res) => {
            info!(log, "Migration task exited: {:?}", res);
            MigrateTaskEvent::TaskExited(res)
        }
        Err(join_err) => {
            if join_err.is_cancelled() {
                panic!("Migration task canceled");
            } else {
                panic!("Migration task panicked: {:?}", join_err.into_panic());
            }
        }
    }
}
