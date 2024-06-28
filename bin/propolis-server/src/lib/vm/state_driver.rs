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
    InstanceMigrateInitiateResponse, InstanceProperties,
    InstanceSpecEnsureRequest, InstanceState, MigrationState,
};
use slog::{error, info};
use uuid::Uuid;

use crate::{
    initializer::{
        build_instance, MachineInitializer, MachineInitializerState,
    },
    migrate::MigrateRole,
    vm::{
        migrate_commands::MigrateTargetCommand, objects::InputVmObjects,
        state_publisher::ExternalStateUpdate,
    },
};

use super::{
    guest_event::{self, GuestEvent},
    migrate_commands::{
        MigrateSourceCommand, MigrateSourceResponse, MigrateTargetResponse,
        MigrateTaskEvent,
    },
    objects::VmObjects,
    request_queue::{ExternalRequest, InstanceAutoStart},
    state_publisher::{MigrationStateUpdate, StatePublisher},
    VmError,
};

#[derive(Debug, PartialEq, Eq)]
enum HandleEventOutcome {
    Continue,
    Exit,
}

/// A reason for starting a VM.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum VmStartReason {
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
    fn new(log: slog::Logger, auto_start: InstanceAutoStart) -> Self {
        Self {
            external_requests: super::request_queue::ExternalRequestQueue::new(
                log, auto_start,
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
    pub(super) fn new(
        log: slog::Logger,
        auto_start: InstanceAutoStart,
    ) -> Self {
        Self {
            inner: Mutex::new(InputQueueInner::new(log, auto_start)),
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
    objects: Arc<VmObjects>,
    input_queue: Arc<InputQueue>,
    external_state: StatePublisher,
    paused: bool,
    migration_src_state: crate::migrate::source::PersistentState,
}

pub(super) struct StateDriverOutput {
    pub state_publisher: StatePublisher,
    pub final_state: InstanceState,
}

pub(super) async fn run_state_driver(
    log: slog::Logger,
    vm: Arc<super::Vm>,
    mut state_publisher: StatePublisher,
    ensure_request: InstanceSpecEnsureRequest,
    ensure_result_tx: tokio::sync::oneshot::Sender<
        Result<propolis_api_types::InstanceEnsureResponse, VmError>,
    >,
    ensure_options: super::EnsureOptions,
) -> StateDriverOutput {
    let migration_in_id =
        ensure_request.migrate.as_ref().map(|req| req.migration_id);

    let input_queue = Arc::new(InputQueue::new(
        log.new(slog::o!("component" => "request_queue")),
        match &ensure_request.migrate {
            Some(_) => InstanceAutoStart::Yes,
            None => InstanceAutoStart::No,
        },
    ));

    let vm_objects = match build_vm(
        &log,
        &vm,
        &ensure_request,
        &ensure_options,
        &input_queue,
        &mut state_publisher,
    )
    .await
    {
        Ok(objects) => objects,
        Err((e, objects)) => {
            state_publisher
                .update(ExternalStateUpdate::Instance(InstanceState::Failed));
            vm.start_failed(objects.is_some()).await;
            let _ =
                ensure_result_tx.send(Err(VmError::InitializationFailed(e)));
            return StateDriverOutput {
                state_publisher,
                final_state: InstanceState::Failed,
            };
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
    vm.make_active(&log, input_queue.clone(), &vm_objects, services).await;
    let _ =
        ensure_result_tx.send(Ok(propolis_api_types::InstanceEnsureResponse {
            migrate: migration_in_id
                .map(|id| InstanceMigrateInitiateResponse { migration_id: id }),
        }));

    let state_driver = StateDriver {
        log,
        objects: vm_objects,
        input_queue,
        external_state: state_publisher,
        paused: false,
        migration_src_state: Default::default(),
    };

    let state_publisher = state_driver.run(migration_in_id.is_some()).await;
    vm.set_rundown().await;
    StateDriverOutput { state_publisher, final_state: InstanceState::Destroyed }
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

        let start_result = self.objects.write().await.start(start_reason).await;
        match &start_result {
            Ok(()) => {
                self.publish_steady_state(InstanceState::Running);
            }
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

        self.objects.write().await.reboot().await;

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

        {
            let mut guard = self.objects.write().await;

            // Entities expect to be paused before being halted. Note that the VM
            // may be paused already if it is being torn down after a successful
            // migration out.
            if !self.paused {
                guard.pause().await;
                self.paused = true;
            }

            guard.halt().await;
        }

        self.publish_steady_state(InstanceState::Stopped);
    }

    async fn pause(&mut self) {
        assert!(!self.paused);
        self.objects.write().await.pause().await;
        self.paused = true;
    }

    async fn resume(&mut self) {
        assert!(self.paused);
        self.objects.write().await.resume();
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
        let objects_for_task = self.objects.clone();
        let mut migrate_task = tokio::spawn(async move {
            crate::migrate::source::migrate(
                objects_for_task,
                command_tx,
                response_rx,
                migrate_ctx.conn,
                migrate_ctx.protocol,
            )
            .await
        });

        // The migration task may try to acquire the VM object lock shared, so
        // this task cannot hold it excluive while waiting for the migration
        // task to send an event.
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
                        self.input_queue
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

        fn spec_element_not_found(disk_name: &str) -> dropshot::HttpError {
            let msg = format!("Crucible backend for {:?} not found", disk_name);
            dropshot::HttpError::for_not_found(Some(msg.clone()), msg)
        }

        let mut objects = self.objects.write().await;
        let (readonly, old_vcr_json) = {
            let StorageBackendV0::Crucible(bes) = objects
                .instance_spec()
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
            let backend = objects
                .crucible_backends()
                .get(backend_id)
                .ok_or_else(|| {
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
            .instance_spec_mut()
            .backends
            .storage_backends
            .insert(disk_name, new_bes);

        info!(self.log, "replaced Crucible VCR"; "backend_id" => %backend_id);

        Ok(replace_result)
    }
}

async fn build_vm(
    log: &slog::Logger,
    parent: &Arc<super::Vm>,
    request: &InstanceSpecEnsureRequest,
    options: &super::EnsureOptions,
    input_queue: &Arc<InputQueue>,
    state_publisher: &mut StatePublisher,
) -> anyhow::Result<Arc<VmObjects>, (anyhow::Error, Option<Arc<VmObjects>>)> {
    // If the caller didn't ask to initialize by live migration in, immediately
    // create the VM objects and return them.
    let Some(migrate_request) = &request.migrate else {
        let input_objects = initialize_vm_objects_from_spec(
            log,
            input_queue,
            &request.properties,
            &request.instance_spec,
            options,
        )
        .await
        .map_err(|e| (e, None))?;

        let vm_objects = Arc::new(VmObjects::new(
            log.clone(),
            parent.clone(),
            input_objects,
        ));

        return Ok(vm_objects);
    };

    // The caller has asked to initialize by live migration in. Initialize VM
    // objects at the live migration task's request.
    //
    // Begin by contacting the source Propolis and obtaining the connection that
    // the actual migration task will need.
    let migrate_ctx = crate::migrate::dest_initiate(
        log,
        migrate_request,
        options.local_server_addr,
    )
    .await
    .map_err(|e| (e.into(), None))?;

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

    let init_command = command_rx.recv().await.ok_or_else(|| {
        (anyhow::anyhow!("migration task unexpectedly closed channel"), None)
    })?;

    let input_objects = 'init: {
        let MigrateTargetCommand::InitializeFromExternalSpec = init_command
        else {
            error!(log, "migration protocol didn't init objects first";
                   "command" => ?init_command);
            break 'init Err(anyhow::anyhow!(
                "migration protocol didn't init objects first"
            ));
        };

        initialize_vm_objects_from_spec(
            log,
            input_queue,
            &request.properties,
            &request.instance_spec,
            options,
        )
        .await
        .map_err(Into::into)
    };

    let vm_objects = match input_objects {
        Ok(o) => Arc::new(VmObjects::new(log.clone(), parent.clone(), o)),
        Err(e) => {
            let _ = response_tx
                .send(MigrateTargetResponse::VmObjectsInitialized(Err(
                    e.to_string()
                )))
                .await;
            state_publisher.update(ExternalStateUpdate::Migration(
                MigrationStateUpdate {
                    id: migrate_ctx.migration_id,
                    state: MigrationState::Error,
                    role: MigrateRole::Source,
                },
            ));

            return Err((e, None));
        }
    };

    // The migration task imports device state by operating directly on the
    // newly-created VM objects. Before sending them to the task, make sure the
    // objects are ready to have state imported into them. Specifically, ensure
    // that the VM's vCPUs are activated so they can enter the guest after
    // migration and pause the kernel VM to allow it to import device state
    // consistently.
    //
    // Drop the lock after this operation so that the migration task can acquire
    // it.
    {
        let guard = vm_objects.read().await;
        guard.reset_vcpus();
        guard.pause_kernel_vm();
    }

    if response_tx
        .send(MigrateTargetResponse::VmObjectsInitialized(Ok(
            vm_objects.clone()
        )))
        .await
        .is_err()
    {
        vm_objects.write().await.resume_kernel_vm();
        return Err((
            anyhow::anyhow!("migration task unexpectedly closed channel"),
            Some(vm_objects),
        ));
    }

    loop {
        let action =
            next_migrate_task_event(&mut migrate_task, &mut command_rx, log)
                .await;

        match action {
            MigrateTaskEvent::TaskExited(res) => match res {
                Ok(()) => {
                    return Ok(vm_objects);
                }
                Err(e) => {
                    error!(log, "target migration task failed";
                           "error" => %e);

                    vm_objects.write().await.resume_kernel_vm();
                    return Err((e.into(), Some(vm_objects)));
                }
            },
            MigrateTaskEvent::Command(MigrateTargetCommand::UpdateState(
                state,
            )) => {
                state_publisher.update(ExternalStateUpdate::Migration(
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

async fn initialize_vm_objects_from_spec(
    log: &slog::Logger,
    event_queue: &Arc<InputQueue>,
    properties: &InstanceProperties,
    spec: &VersionedInstanceSpec,
    options: &super::EnsureOptions,
) -> anyhow::Result<InputVmObjects> {
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

    Ok(InputVmObjects {
        instance_spec: v0_spec.clone(),
        vcpu_tasks,
        machine,
        lifecycle_components: devices,
        block_backends,
        crucible_backends,
        com1,
        framebuffer: Some(ramfb),
        ps2ctrl,
    })
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
