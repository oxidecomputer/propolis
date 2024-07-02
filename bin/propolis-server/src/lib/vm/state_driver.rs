// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A task to handle requests to change a VM's state or configuration.

use std::{
    sync::{Arc, Condvar, Mutex},
    time::Duration,
};

use propolis_api_types::{
    instance_spec::{
        components::backends::CrucibleStorageBackend, v0::StorageBackendV0,
    },
    InstanceMigrateInitiateResponse, InstanceSpecEnsureRequest, InstanceState,
    MigrationState,
};
use slog::{error, info};
use uuid::Uuid;

use crate::{migrate::MigrateRole, vm::state_publisher::ExternalStateUpdate};

use super::{
    guest_event::{self, GuestEvent},
    migrate_commands::{
        next_migrate_task_event, MigrateSourceCommand, MigrateSourceResponse,
        MigrateTaskEvent,
    },
    objects::VmObjects,
    request_queue::{ExternalRequest, InstanceAutoStart},
    startup::BuildVmOutput,
    state_publisher::{MigrationStateUpdate, StatePublisher},
    VmError,
};

/// Tells the state driver what to do after handling an event.
#[derive(Debug, PartialEq, Eq)]
enum HandleEventOutcome {
    Continue,
    Exit { final_state: InstanceState },
}

/// A reason for starting a VM.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum VmStartReason {
    MigratedIn,
    ExplicitRequest,
}

/// A kind of event the state driver can handle.
#[derive(Debug)]
enum InputQueueEvent {
    ExternalRequest(ExternalRequest),
    GuestEvent(GuestEvent),
}

/// The lock-guarded parts of a state driver's input queue.
struct InputQueueInner {
    /// State change requests from the external API.
    external_requests: super::request_queue::ExternalRequestQueue,

    /// State change requests from the VM's components. These take precedence
    /// over external state change requests.
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

/// A queue for external state change requests and guest-driven state changes.
pub(super) struct InputQueue {
    inner: Mutex<InputQueueInner>,
    cv: Condvar,
}

impl InputQueue {
    /// Creates a new state driver input queue.
    pub(super) fn new(
        log: slog::Logger,
        auto_start: InstanceAutoStart,
    ) -> Self {
        Self {
            inner: Mutex::new(InputQueueInner::new(log, auto_start)),
            cv: Condvar::new(),
        }
    }

    /// Waits for an event to arrive on the input queue and returns it for
    /// processing.
    ///
    /// External requests and guest events are stored in separate queues. If
    /// both queues have events when this routine is called, the guest event
    /// queue takes precedence.
    fn wait_for_next_event(&self) -> InputQueueEvent {
        // `block_in_place` is required to avoid blocking the executor while
        // waiting on the condvar.
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

    /// Notifies the external request queue that the instance's state has
    /// changed so that it can change the dispositions for new state change
    /// requests.
    fn notify_instance_state_change(
        &self,
        state: super::request_queue::InstanceStateChange,
    ) {
        let mut guard = self.inner.lock().unwrap();
        guard.external_requests.notify_instance_state_change(state);
    }

    /// Submits an external state change request to the queue.
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

impl guest_event::VcpuEventHandler for InputQueue {
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

/// The context for a VM state driver task's main loop.
struct StateDriver {
    /// The state driver's associated logger.
    log: slog::Logger,

    /// The VM objects this driver is managing.
    objects: Arc<VmObjects>,

    /// The input queue this driver gets events from.
    input_queue: Arc<InputQueue>,

    /// The channel to which this driver publishes external instance state
    /// changes.
    external_state: StatePublisher,

    /// True if the VM is paused.
    paused: bool,

    /// State persisted from previous attempts to migrate out of this VM.
    migration_src_state: crate::migrate::source::PersistentState,
}

/// The values returned by a state driver task when it exits.
pub(super) struct StateDriverOutput {
    /// The channel this driver used to publish external instance state changes.
    pub state_publisher: StatePublisher,

    /// The terminal state of this instance. When the instance completes
    /// rundown, the parent VM publishes this state to the associated channel.
    pub final_state: InstanceState,
}

/// Creates a new set of VM objects in response to an `ensure_request` directed
/// to the supplied `vm`.
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

    let BuildVmOutput { vm_objects, migration_in } =
        match super::startup::build_vm(
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
                state_publisher.update(ExternalStateUpdate::Instance(
                    InstanceState::Failed,
                ));

                vm.vm_init_failed(objects.is_some()).await;
                let _ = ensure_result_tx
                    .send(Err(VmError::InitializationFailed(e)));
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

    // If the VM was initialized via migration in, complete that migration now.
    //
    // External callers who ask to initialize an instance via migration in
    // expect their API calls to complete once the relevant VM is initialized
    // and the migration task has started (as opposed to when the entire
    // migration attempt has completed), so this must happen after the ensure
    // result is published. (Note that it's OK for the migration to fail after
    // this point: the ensure request succeeds, but the instance goes to the
    // Failed state and the migration appears to have failed.)
    if let Some(migration_in) = migration_in {
        if let Err(e) = migration_in.run(&mut state_publisher).await {
            error!(log, "inbound live migration task failed";
                   "error" => ?e);

            vm.set_rundown().await;
            return StateDriverOutput {
                state_publisher,
                final_state: InstanceState::Failed,
            };
        }
    }

    let state_driver = StateDriver {
        log,
        objects: vm_objects,
        input_queue,
        external_state: state_publisher,
        paused: false,
        migration_src_state: Default::default(),
    };

    // Run the VM until it exits, then set rundown on the parent VM so that no
    // new external callers can access its objects or services.
    let output = state_driver.run(migration_in_id.is_some()).await;
    vm.set_rundown().await;
    output
}

impl StateDriver {
    pub(super) async fn run(mut self, migrated_in: bool) -> StateDriverOutput {
        info!(self.log, "state driver launched");

        let final_state = if migrated_in {
            if self.start_vm(VmStartReason::MigratedIn).await.is_ok() {
                self.run_loop().await
            } else {
                InstanceState::Failed
            }
        } else {
            self.run_loop().await
        };

        StateDriverOutput { state_publisher: self.external_state, final_state }
    }

    async fn run_loop(&mut self) -> InstanceState {
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
            match outcome {
                HandleEventOutcome::Continue => {}
                HandleEventOutcome::Exit { final_state } => {
                    info!(self.log, "state driver exiting";
                          "final_state" => ?final_state);

                    return final_state;
                }
            }
        }
    }

    async fn start_vm(
        &mut self,
        start_reason: VmStartReason,
    ) -> anyhow::Result<()> {
        info!(self.log, "starting instance"; "reason" => ?start_reason);

        let start_result =
            self.objects.lock_exclusive().await.start(start_reason).await;
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
                HandleEventOutcome::Exit {
                    final_state: InstanceState::Destroyed,
                }
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
                HandleEventOutcome::Exit {
                    final_state: InstanceState::Destroyed,
                }
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
                    Err(_) => HandleEventOutcome::Exit {
                        final_state: InstanceState::Failed,
                    },
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
                HandleEventOutcome::Exit {
                    final_state: InstanceState::Destroyed,
                }
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

        self.objects.lock_exclusive().await.reboot().await;

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
            let mut guard = self.objects.lock_exclusive().await;

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
        self.objects.lock_exclusive().await.pause().await;
        self.paused = true;
    }

    async fn resume(&mut self) {
        assert!(self.paused);
        self.objects.lock_exclusive().await.resume();
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

        let mut objects = self.objects.lock_exclusive().await;
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
