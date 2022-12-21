use std::sync::Arc;

use crate::migrate::MigrateError;

use super::{
    ExternalRequest, GuestEvent, LifecycleStage, MigrateSourceCommand,
    MigrateSourceResponse, MigrateTargetCommand, MigrateTaskEvent,
    StartupStage, StateDriverEvent,
};

use propolis_client::handmade::{
    api::InstanceState as ApiInstanceState,
    api::MigrationState as ApiMigrationState,
};
use slog::{info, Logger};
use uuid::Uuid;

pub(super) struct StateDriver<C: super::StateDriverCommandSink> {
    /// A handle to the host server's tokio runtime, useful for spawning tasks
    /// that need to interact with async code (e.g. spinning up migration
    /// tasks).
    runtime_hdl: tokio::runtime::Handle,

    /// A reference to the command sink to which this driver should send its
    /// requests to send messages to entities or update other VM controller
    /// state.
    controller: Arc<C>,

    /// The bundle of tasks that execute the vCPU run loops for this VM's vCPUs.
    vcpu_tasks: crate::vcpu_tasks::VcpuTasks,

    /// The state worker's logger.
    log: Logger,

    /// The generation number to use when publishing externally-visible state
    /// updates.
    state_gen: u64,

    /// Whether the worker's VM's entities are paused.
    paused: bool,
}

impl<C: super::StateDriverCommandSink> StateDriver<C> {
    pub(super) fn new(
        runtime_hdl: tokio::runtime::Handle,
        controller: Arc<C>,
        vcpu_tasks: crate::vcpu_tasks::VcpuTasks,
        log: Logger,
        state_gen: u64,
    ) -> Self {
        Self {
            runtime_hdl,
            controller,
            vcpu_tasks,
            log,
            state_gen,
            paused: false,
        }
    }

    /// Manages an instance's lifecycle once it has moved to the Running state.
    pub(super) fn run_state_worker(&mut self) {
        info!(self.log, "State worker launched");

        // Allow actions to queue up state changes that are applied on the next
        // loop iteration.
        let mut next_external: Option<ApiInstanceState> = None;
        let mut next_lifecycle: Option<LifecycleStage> = None;
        loop {
            if let Some(next_lifecycle) = next_lifecycle.take() {
                self.controller.set_lifecycle_stage(next_lifecycle);
            }

            // Update the state visible to external threads querying the state
            // of this instance. If the instance is now logically stopped, this
            // thread must exit (in part to drop its reference to its VM
            // controller).
            //
            // N.B. This check must be last, because it breaks out of the loop.
            if let Some(next_external) = next_external.take() {
                self.controller
                    .update_external_state(&mut self.state_gen, next_external);
                if matches!(next_external, ApiInstanceState::Stopped) {
                    break;
                }
            }

            let event = self.controller.wait_for_next_event();
            let next_action = match event {
                StateDriverEvent::Guest(guest_event) => {
                    self.handle_guest_event(guest_event);
                    continue;
                }
                StateDriverEvent::External(external_event) => external_event,
            };

            match next_action {
                ExternalRequest::MigrateAsTarget {
                    migration_id,
                    task,
                    start_tx,
                    command_rx,
                } => {
                    self.controller.update_external_state(
                        &mut self.state_gen,
                        ApiInstanceState::Migrating,
                    );

                    // The controller API should have updated the lifecycle
                    // stage prior to queuing this work, and neither it nor the
                    // worker should have allowed any other stage to be written
                    // at this point. (Specifically, the API shouldn't allow a
                    // migration in to start after the VM enters the "running"
                    // portion of its lifecycle, from which the worker may need
                    // to tweak lifecycle stages itself.)
                    assert!(matches!(
                        self.controller.get_lifecycle_stage(),
                        LifecycleStage::NotStarted(
                            StartupStage::MigratePending
                        )
                    ));

                    // Ensure the vCPUs are activated properly so that they can
                    // enter the guest after migration. Do this before launching
                    // the migration task so that reset doesn't overwrite the
                    // state written by migration.
                    self.reset_vcpus();
                    next_lifecycle = Some(
                        match self.migrate_as_target(
                            migration_id,
                            task,
                            start_tx,
                            command_rx,
                        ) {
                            Ok(_) => LifecycleStage::NotStarted(
                                StartupStage::Migrated,
                            ),
                            Err(_) => LifecycleStage::NotStarted(
                                StartupStage::MigrationFailed,
                            ),
                        },
                    );
                }
                ExternalRequest::Start { reset_required } => {
                    info!(self.log, "Starting instance";
                          "reset" => reset_required);

                    // Requests to start should put the instance into the
                    // 'starting' stage. Reflect this out to callers who ask
                    // what state the VM is in.
                    self.controller.update_external_state(
                        &mut self.state_gen,
                        ApiInstanceState::Starting,
                    );
                    next_external = Some(ApiInstanceState::Running);
                    next_lifecycle = Some(LifecycleStage::Active);

                    if reset_required {
                        self.reset_vcpus();
                    }

                    // TODO(#209) Transition to a "failed" state here instead of
                    // expecting.
                    self.controller
                        .start_entities()
                        .expect("entity start callout failed");
                    self.vcpu_tasks.resume_all();
                }
                ExternalRequest::Reboot => {
                    next_external = self.do_reboot();
                }
                ExternalRequest::MigrateAsSource {
                    migration_id,
                    task,
                    start_tx,
                    command_rx,
                    response_tx,
                } => {
                    self.controller.update_external_state(
                        &mut self.state_gen,
                        ApiInstanceState::Migrating,
                    );

                    let result = self.migrate_as_source(
                        migration_id,
                        task,
                        start_tx,
                        command_rx,
                        response_tx,
                    );
                    self.controller.finish_migrate_as_source(result);
                }
                ExternalRequest::Stop => {
                    (next_external, next_lifecycle) = self.do_halt();
                }
            }
        }

        info!(self.log, "State worker exiting");
    }

    fn handle_guest_event(
        &mut self,
        event: GuestEvent,
    ) -> (Option<ApiInstanceState>, Option<LifecycleStage>) {
        match event {
            GuestEvent::VcpuSuspendHalt(vcpu_id) => {
                info!(
                    self.log,
                    "Halting due to halt event on vCPU {}", vcpu_id
                );
                self.do_halt()
            }
            GuestEvent::VcpuSuspendReset(vcpu_id) => {
                info!(
                    self.log,
                    "Resetting due to reset event on vCPU {}", vcpu_id
                );
                (self.do_reboot(), None)
            }
            GuestEvent::VcpuSuspendTripleFault(vcpu_id) => {
                info!(
                    self.log,
                    "Resetting due to triple fault on vCPU {}", vcpu_id
                );
                (self.do_reboot(), None)
            }
            GuestEvent::ChipsetHalt => {
                info!(self.log, "Halting due to chipset-driven halt");
                self.do_halt()
            }
            GuestEvent::ChipsetReset => {
                info!(self.log, "Resetting due to chipset-driven reset");
                (self.do_reboot(), None)
            }
        }
    }

    fn do_reboot(&mut self) -> Option<ApiInstanceState> {
        info!(self.log, "Resetting instance");

        // Reboots should only arrive after an instance has started.
        assert!(matches!(
            self.controller.get_lifecycle_stage(),
            LifecycleStage::Active
        ));

        self.controller.update_external_state(
            &mut self.state_gen,
            ApiInstanceState::Rebooting,
        );

        let next_external = Some(ApiInstanceState::Running);

        // Reboot is implemented as a pause -> reset -> resume transition.
        //
        // First, pause the vCPUs and all entities so no partially-completed
        // work is present.
        self.vcpu_tasks.pause_all();
        self.controller.pause_entities();

        // Reset all the entities and the VM's bhyve state, then reset the
        // vCPUs. The vCPU reset must come after the bhyve reset.
        self.controller.reset_entities_and_machine();
        self.reset_vcpus();

        // Resume entities so they're ready to do more work, then
        // resume vCPUs.
        self.controller.resume_entities();
        self.vcpu_tasks.resume_all();

        next_external
    }

    fn do_halt(
        &mut self,
    ) -> (Option<ApiInstanceState>, Option<LifecycleStage>) {
        info!(self.log, "Stopping instance");
        self.controller.update_external_state(
            &mut self.state_gen,
            ApiInstanceState::Stopping,
        );
        let next_external = Some(ApiInstanceState::Stopped);
        let next_lifecycle = Some(LifecycleStage::NoLongerActive);
        self.vcpu_tasks.exit_all();
        self.controller.halt_entities();
        (next_external, next_lifecycle)
    }

    fn migrate_as_target(
        &self,
        migration_id: Uuid,
        mut task: tokio::task::JoinHandle<Result<(), MigrateError>>,
        start_tx: tokio::sync::oneshot::Sender<()>,
        mut command_rx: tokio::sync::mpsc::Receiver<MigrateTargetCommand>,
    ) -> Result<(), MigrateError> {
        start_tx.send(()).unwrap();
        loop {
            let action = self.runtime_hdl.block_on(async {
                Self::next_migrate_task_event(
                    &mut task,
                    &mut command_rx,
                    &self.log,
                )
                .await
            });

            match action {
                MigrateTaskEvent::TaskExited(res) => {
                    if res.is_err() {
                        self.controller.set_migration_state(
                            migration_id,
                            ApiMigrationState::Error,
                        );
                    } else {
                        assert!(matches!(
                            self.controller.get_migration_state(),
                            Some((_, ApiMigrationState::Finish))
                        ));
                    }
                    return res;
                }
                MigrateTaskEvent::Command(
                    MigrateTargetCommand::UpdateState(state),
                ) => {
                    self.controller.set_migration_state(migration_id, state);
                }
            }
        }
    }

    fn migrate_as_source(
        &mut self,
        migration_id: Uuid,
        mut task: tokio::task::JoinHandle<Result<(), MigrateError>>,
        start_tx: tokio::sync::oneshot::Sender<()>,
        mut command_rx: tokio::sync::mpsc::Receiver<MigrateSourceCommand>,
        response_tx: tokio::sync::mpsc::Sender<MigrateSourceResponse>,
    ) -> Result<(), MigrateError> {
        start_tx.send(()).unwrap();

        // Wait either for the migration task to exit or for it to ask the
        // worker to pause or resume the instance's entities.
        loop {
            let action = self.runtime_hdl.block_on(async {
                Self::next_migrate_task_event(
                    &mut task,
                    &mut command_rx,
                    &self.log,
                )
                .await
            });

            match action {
                // If the task exited, bubble its result back up to the main
                // state worker loop to decide on the instance's next state.
                //
                // If migration failed while entities were paused, this instance
                // is allowed to resume, so resume its components here.
                MigrateTaskEvent::TaskExited(res) => {
                    if res.is_err() {
                        if self.paused {
                            self.controller.resume_entities();
                            self.vcpu_tasks.resume_all();
                            self.paused = false;
                            self.controller.set_migration_state(
                                migration_id,
                                ApiMigrationState::Error,
                            );
                        }
                    } else {
                        assert!(matches!(
                            self.controller.get_migration_state(),
                            Some((_, ApiMigrationState::Finish))
                        ));
                    }
                    return res;
                }
                MigrateTaskEvent::Command(cmd) => match cmd {
                    MigrateSourceCommand::UpdateState(state) => {
                        self.controller
                            .set_migration_state(migration_id, state);
                    }
                    MigrateSourceCommand::Pause => {
                        self.vcpu_tasks.pause_all();
                        self.controller.pause_entities();
                        self.paused = true;
                        response_tx
                            .blocking_send(MigrateSourceResponse::Pause(Ok(())))
                            .unwrap();
                    }
                },
            }
        }
    }

    async fn next_migrate_task_event<E>(
        task: &mut tokio::task::JoinHandle<Result<(), MigrateError>>,
        command_rx: &mut tokio::sync::mpsc::Receiver<E>,
        log: &Logger,
    ) -> MigrateTaskEvent<E> {
        if let Some(cmd) = command_rx.recv().await {
            return MigrateTaskEvent::Command(cmd);
        }

        // The sender side of the command channel is dropped, which means the
        // migration task is exiting. Wait for it to finish and snag its result.
        match task.await {
            Ok(res) => {
                info!(log, "Migration source task exited: {:?}", res);
                MigrateTaskEvent::TaskExited(res)
            }
            Err(join_err) => {
                if join_err.is_cancelled() {
                    panic!("Migration task canceled");
                } else {
                    panic!(
                        "Migration task panicked: {:?}",
                        join_err.into_panic()
                    );
                }
            }
        }
    }

    fn reset_vcpus(&self) {
        self.vcpu_tasks.new_generation();
        self.controller.reset_vcpu_state();
    }
}
