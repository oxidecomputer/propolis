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

use crate::vcpu_tasks::VcpuTaskController;

pub(super) struct StateDriver<
    V: super::StateDriverVmController,
    C: VcpuTaskController,
> {
    /// A handle to the host server's tokio runtime, useful for spawning tasks
    /// that need to interact with async code (e.g. spinning up migration
    /// tasks).
    runtime_hdl: tokio::runtime::Handle,

    /// A reference to the command sink to which this driver should send its
    /// requests to send messages to entities or update other VM controller
    /// state.
    controller: Arc<V>,

    /// The controller for this instance's vCPU tasks.
    vcpu_tasks: C,

    /// The state worker's logger.
    log: Logger,

    /// The generation number to use when publishing externally-visible state
    /// updates.
    state_gen: u64,

    /// Whether the worker's VM's entities are paused.
    paused: bool,
}

impl<V, C> StateDriver<V, C>
where
    V: super::StateDriverVmController,
    C: VcpuTaskController,
{
    pub(super) fn new(
        runtime_hdl: tokio::runtime::Handle,
        controller: Arc<V>,
        vcpu_tasks: C,
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
            self.handle_event(event, &mut next_external, &mut next_lifecycle);
        }

        info!(self.log, "State worker exiting");
    }

    fn handle_event(
        &mut self,
        event: StateDriverEvent,
        next_external: &mut Option<ApiInstanceState>,
        next_lifecycle: &mut Option<LifecycleStage>,
    ) {
        let next_action = match event {
            StateDriverEvent::Guest(guest_event) => {
                (*next_external, *next_lifecycle) =
                    self.handle_guest_event(guest_event);
                return;
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
                    LifecycleStage::NotStarted(StartupStage::MigratePending)
                ));

                // Ensure the vCPUs are activated properly so that they can
                // enter the guest after migration. Do this before launching
                // the migration task so that reset doesn't overwrite the
                // state written by migration.
                self.reset_vcpus();
                *next_lifecycle = Some(
                    match self.migrate_as_target(
                        migration_id,
                        task,
                        start_tx,
                        command_rx,
                    ) {
                        Ok(_) => {
                            LifecycleStage::NotStarted(StartupStage::Migrated)
                        }
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
                *next_external = Some(ApiInstanceState::Running);
                *next_lifecycle = Some(LifecycleStage::Active);

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
                *next_external = self.do_reboot();
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
                (*next_external, *next_lifecycle) = self.do_halt();
            }
        }
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

        // Entities expect to be paused before being halted. Note that the VM
        // may be paused already if it is being torn down after a successful
        // migration out.
        if !self.paused {
            self.vcpu_tasks.pause_all();
            self.controller.pause_entities();
            self.paused = true;
        }

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

#[cfg(test)]
mod tests {
    use mockall::Sequence;

    use super::*;
    use crate::vcpu_tasks::MockVcpuTaskController;
    use crate::vm::MockStateDriverVmController;

    fn make_state_driver(
        vm_ctrl: MockStateDriverVmController,
        vcpu_ctrl: MockVcpuTaskController,
    ) -> StateDriver<MockStateDriverVmController, MockVcpuTaskController> {
        let logger = slog::Logger::root(slog::Discard, slog::o!());
        StateDriver::new(
            tokio::runtime::Handle::current(),
            Arc::new(vm_ctrl),
            vcpu_ctrl,
            logger,
            0,
        )
    }

    /// Generates default mocks for the VM controller and vCPU task controller
    /// that accept unlimited requests to read state.
    fn make_default_mocks(
    ) -> (MockStateDriverVmController, MockVcpuTaskController) {
        let mut vm_ctrl = MockStateDriverVmController::new();
        let vcpu_ctrl = MockVcpuTaskController::new();

        vm_ctrl
            .expect_get_lifecycle_stage()
            .returning(|| LifecycleStage::Active);
        vm_ctrl.expect_get_migration_state().returning(|| None);

        (vm_ctrl, vcpu_ctrl)
    }

    fn make_migration_source_mocks(
        migration_id: Uuid,
    ) -> (MockStateDriverVmController, MockVcpuTaskController) {
        let mut vm_ctrl = MockStateDriverVmController::new();
        let vcpu_ctrl = MockVcpuTaskController::new();

        vm_ctrl
            .expect_get_lifecycle_stage()
            .returning(|| LifecycleStage::Active);
        vm_ctrl
            .expect_get_migration_state()
            .returning(move || Some((migration_id, ApiMigrationState::Finish)));

        (vm_ctrl, vcpu_ctrl)
    }

    /* TODO(#250)
    fn make_migration_target_mocks(
        migration_id: Uuid,
    ) -> (MockStateDriverVmController, MockVcpuTaskController) {
        let mut vm_ctrl = MockStateDriverVmController::new();
        let vcpu_ctrl = MockVcpuTaskController::new();

        vm_ctrl.expect_get_lifecycle_stage().returning(|| {
            LifecycleStage::NotStarted(StartupStage::MigratePending)
        });
        vm_ctrl
            .expect_get_migration_state()
            .returning(move || Some((migration_id, ApiMigrationState::Finish)));

        (vm_ctrl, vcpu_ctrl)
    }
    */

    fn add_reboot_expectations(
        vm_ctrl: &mut MockStateDriverVmController,
        vcpu_ctrl: &mut MockVcpuTaskController,
    ) {
        vm_ctrl
            .expect_update_external_state()
            .times(1)
            .withf(|&_gen, &state| state == ApiInstanceState::Rebooting)
            .returning(|_, _| ());

        // The reboot process requires careful ordering of steps to make sure
        // the VM's vCPUs are put into the correct state when the machine starts
        // up.
        let mut seq = Sequence::new();

        // First, reboot has to pause everything. It doesn't actually matter
        // whether vCPUs or entities pause first, but there's no way to specify
        // that these events must be sequenced before other expectations but
        // have no ordering with respect to each other.
        vcpu_ctrl
            .expect_pause_all()
            .times(1)
            .in_sequence(&mut seq)
            .returning(|| ());
        vm_ctrl
            .expect_pause_entities()
            .times(1)
            .in_sequence(&mut seq)
            .returning(|| ());

        // The entities and--importantly--the bhyve VM itself must be reset
        // before resetting any vCPU state (so that bhyve will accept the ioctls
        // sent to the vCPUs during the reset process).
        vm_ctrl
            .expect_reset_entities_and_machine()
            .times(1)
            .in_sequence(&mut seq)
            .returning(|| ());
        vcpu_ctrl
            .expect_new_generation()
            .times(1)
            .in_sequence(&mut seq)
            .returning(|| ());
        vm_ctrl
            .expect_reset_vcpu_state()
            .times(1)
            .in_sequence(&mut seq)
            .returning(|| ());

        // Entities and vCPUs can technically be resumed in either order, but
        // resuming entities first allows them to be ready when the vCPUs start
        // creating work for them to do.
        vm_ctrl
            .expect_resume_entities()
            .times(1)
            .in_sequence(&mut seq)
            .returning(|| ());
        vcpu_ctrl
            .expect_resume_all()
            .times(1)
            .in_sequence(&mut seq)
            .returning(|| ());
    }

    #[tokio::test]
    async fn guest_triple_fault_reboots() {
        let (mut vm_ctrl, mut vcpu_ctrl) = make_default_mocks();
        let mut next_external: Option<ApiInstanceState> = None;
        let mut next_lifecycle: Option<LifecycleStage> = None;

        add_reboot_expectations(&mut vm_ctrl, &mut vcpu_ctrl);
        let mut driver = make_state_driver(vm_ctrl, vcpu_ctrl);
        driver.handle_event(
            StateDriverEvent::Guest(GuestEvent::VcpuSuspendTripleFault(0)),
            &mut next_external,
            &mut next_lifecycle,
        );

        assert!(matches!(next_external, Some(ApiInstanceState::Running)));
    }

    #[tokio::test]
    async fn guest_chipset_reset_reboots() {
        let (mut vm_ctrl, mut vcpu_ctrl) = make_default_mocks();
        let mut next_external: Option<ApiInstanceState> = None;
        let mut next_lifecycle: Option<LifecycleStage> = None;

        add_reboot_expectations(&mut vm_ctrl, &mut vcpu_ctrl);
        let mut driver = make_state_driver(vm_ctrl, vcpu_ctrl);
        driver.handle_event(
            StateDriverEvent::Guest(GuestEvent::ChipsetReset),
            &mut next_external,
            &mut next_lifecycle,
        );

        assert!(matches!(next_external, Some(ApiInstanceState::Running)));
    }

    #[tokio::test]
    async fn start_from_cold_boot() {
        let (mut vm_ctrl, mut vcpu_ctrl) = make_default_mocks();
        let mut next_external: Option<ApiInstanceState> = None;
        let mut next_lifecycle: Option<LifecycleStage> = None;

        let mut seq = Sequence::new();
        vm_ctrl
            .expect_update_external_state()
            .times(1)
            .withf(|&_gen, &state| state == ApiInstanceState::Starting)
            .returning(|_, _| ());
        vcpu_ctrl
            .expect_new_generation()
            .times(1)
            .in_sequence(&mut seq)
            .returning(|| ());
        vm_ctrl
            .expect_reset_vcpu_state()
            .times(1)
            .in_sequence(&mut seq)
            .returning(|| ());
        vm_ctrl
            .expect_start_entities()
            .times(1)
            .in_sequence(&mut seq)
            .returning(|| Ok(()));
        vcpu_ctrl
            .expect_resume_all()
            .times(1)
            .in_sequence(&mut seq)
            .returning(|| ());

        let mut driver = make_state_driver(vm_ctrl, vcpu_ctrl);
        driver.handle_event(
            StateDriverEvent::External(ExternalRequest::Start {
                reset_required: true,
            }),
            &mut next_external,
            &mut next_lifecycle,
        );

        assert!(matches!(next_external, Some(ApiInstanceState::Running)));
        assert!(matches!(next_lifecycle, Some(LifecycleStage::Active)));
    }

    #[tokio::test]
    async fn entities_pause_before_halting() {
        let (mut vm_ctrl, mut vcpu_ctrl) = make_default_mocks();
        let mut next_external: Option<ApiInstanceState> = None;
        let mut next_lifecycle: Option<LifecycleStage> = None;

        let mut seq = Sequence::new();
        vm_ctrl
            .expect_update_external_state()
            .times(1)
            .withf(|&_gen, &state| state == ApiInstanceState::Stopping)
            .returning(|_, _| ());
        vcpu_ctrl
            .expect_pause_all()
            .times(1)
            .in_sequence(&mut seq)
            .returning(|| ());
        vm_ctrl
            .expect_pause_entities()
            .times(1)
            .in_sequence(&mut seq)
            .returning(|| ());
        vcpu_ctrl
            .expect_exit_all()
            .times(1)
            .in_sequence(&mut seq)
            .returning(|| ());
        vm_ctrl
            .expect_halt_entities()
            .times(1)
            .in_sequence(&mut seq)
            .returning(|| ());

        let mut driver = make_state_driver(vm_ctrl, vcpu_ctrl);
        driver.handle_event(
            StateDriverEvent::External(ExternalRequest::Stop),
            &mut next_external,
            &mut next_lifecycle,
        );
    }

    #[tokio::test]
    async fn entities_pause_once_when_halting_after_migration_out() {
        let migration_id = Uuid::new_v4();
        let (start_tx, start_rx) = tokio::sync::oneshot::channel();
        let (task_exit_tx, task_exit_rx) = tokio::sync::oneshot::channel();
        let (command_tx, command_rx) = tokio::sync::mpsc::channel(1);
        let (response_tx, mut response_rx) = tokio::sync::mpsc::channel(1);
        let migrate_task = tokio::spawn(async move {
            start_rx.await.unwrap();
            task_exit_rx.await.unwrap()
        });

        let (mut vm_ctrl, mut vcpu_ctrl) =
            make_migration_source_mocks(migration_id);
        let mut next_external: Option<ApiInstanceState> = None;
        let mut next_lifecycle: Option<LifecycleStage> = None;

        // This test will simulate a migration out (with a pause command), then
        // order the state driver to halt. This should produce exactly one set
        // of pause commands and one set of halt commands with no resume
        // commands.
        vm_ctrl.expect_pause_entities().times(1).returning(|| ());
        vcpu_ctrl.expect_pause_all().times(1).returning(|| ());
        vcpu_ctrl.expect_exit_all().times(1).returning(|| ());
        vm_ctrl.expect_halt_entities().times(1).returning(|| ());
        vm_ctrl.expect_resume_entities().never();
        vcpu_ctrl.expect_resume_all().never();

        // The driver may update the external state during this operation.
        // Permit any of these mutations (the specific sequence thereof is
        // out of this test's scope).
        vm_ctrl.expect_update_external_state().times(..).returning(|_, _| ());
        vm_ctrl
            .expect_finish_migrate_as_source()
            .times(1)
            .withf(|res| res.is_ok())
            .returning(|_| ());

        let mut driver = make_state_driver(vm_ctrl, vcpu_ctrl);

        // The state driver expects to run on an OS thread outside the async
        // runtime so that it can call `block_on` to wait for messages from the
        // migration task.
        let hdl = std::thread::spawn(move || {
            driver.handle_event(
                StateDriverEvent::External(ExternalRequest::MigrateAsSource {
                    migration_id,
                    task: migrate_task,
                    start_tx,
                    command_rx,
                    response_tx,
                }),
                &mut next_external,
                &mut next_lifecycle,
            );

            // Return the driver (which has the mocks attached) when the thread
            // is joined so the test can continue using it.
            driver
        });

        // Simulate a pause and the successful completion of migration.
        command_tx.send(MigrateSourceCommand::Pause).await.unwrap();
        let resp = response_rx.recv().await.unwrap();
        assert!(matches!(resp, MigrateSourceResponse::Pause(Ok(()))));

        drop(command_tx);
        task_exit_tx.send(Ok(())).unwrap();

        // Wait for the call to `handle_event` to return before tearing anything
        // else down.
        driver = tokio::task::spawn_blocking(move || hdl.join().unwrap())
            .await
            .unwrap();

        driver.handle_event(
            StateDriverEvent::External(ExternalRequest::Stop),
            &mut next_external,
            &mut next_lifecycle,
        );
    }

    /* TODO(#250)
    #[tokio::test]
    async fn vm_starts_after_migration_in() {
        let migration_id = Uuid::new_v4();
        let (start_tx, start_rx) = tokio::sync::oneshot::channel();
        let (task_exit_tx, task_exit_rx) = tokio::sync::oneshot::channel();
        let (command_tx, command_rx) = tokio::sync::mpsc::channel(1);
        let migrate_task = tokio::spawn(async move {
            start_rx.await.unwrap();
            task_exit_rx.await.unwrap()
        });

        let (mut vm_ctrl, mut vcpu_ctrl) =
            make_migration_target_mocks(migration_id);
        let mut next_external: Option<ApiInstanceState> = None;
        let mut next_lifecycle: Option<LifecycleStage> = None;

        vm_ctrl
            .expect_update_external_state()
            .times(1)
            .withf(|&_gen, &state| state == ApiInstanceState::Migrating)
            .returning(|_, _| ());
        vcpu_ctrl.expect_new_generation().times(1).returning(|| ());
        vm_ctrl.expect_reset_vcpu_state().times(1).returning(|| ());
        vm_ctrl.expect_resume_entities().times(1).returning(|| ());
        vcpu_ctrl.expect_resume_all().times(1).returning(|| ());

        let mut driver = make_state_driver(vm_ctrl, vcpu_ctrl);

        // The state driver expects to run on an OS thread outside the async
        // runtime so that it can call `block_on` to wait for messages from the
        // migration task.
        let hdl = std::thread::spawn(move || {
            driver.handle_event(
                StateDriverEvent::External(ExternalRequest::MigrateAsTarget {
                    migration_id,
                    task: migrate_task,
                    start_tx,
                    command_rx,
                }),
                &mut next_external,
                &mut next_lifecycle,
            )
        });

        // Explicitly drop the command channel to signal to the driver that
        // the migration task is completing.
        drop(command_tx);
        task_exit_tx.send(Ok(())).unwrap();

        // Wait for the call to `handle_event` to return before tearing anything
        // else down.
        tokio::task::spawn_blocking(move || {
            hdl.join().unwrap();
        })
        .await
        .unwrap();
    }
    */

    /* TODO(#252)
    #[tokio::test]
    async fn failed_migration_out_does_not_unpause() {
        let migration_id = Uuid::new_v4();
        let (start_tx, start_rx) = tokio::sync::oneshot::channel();
        let (task_exit_tx, task_exit_rx) = tokio::sync::oneshot::channel();
        let (command_tx, command_rx) = tokio::sync::mpsc::channel(1);
        let (response_tx, mut response_rx) = tokio::sync::mpsc::channel(1);
        let migrate_task = tokio::spawn(async move {
            start_rx.await.unwrap();
            task_exit_rx.await.unwrap()
        });

        let (mut vm_ctrl, mut vcpu_ctrl) = make_default_mocks();
        let mut next_external: Option<ApiInstanceState> = None;
        let mut next_lifecycle: Option<LifecycleStage> = None;

        vm_ctrl
            .expect_update_external_state()
            .times(1)
            .withf(|&_gen, &state| state == ApiInstanceState::Migrating)
            .returning(|_, _| ());
        vm_ctrl.expect_pause_entities().times(1).returning(|| ());
        vcpu_ctrl.expect_pause_all().times(1).returning(|| ());

        let expected_migration_id = migration_id;
        vm_ctrl
            .expect_set_migration_state()
            .times(1)
            .withf(move |&id, &state| {
                id == expected_migration_id && state == ApiMigrationState::Error
            })
            .returning(|_, _| ());

        vm_ctrl.expect_resume_entities().never();
        vcpu_ctrl.expect_resume_all().never();

        let mut driver = make_state_driver(vm_ctrl, vcpu_ctrl);
        driver.handle_event(
            StateDriverEvent::External(ExternalRequest::MigrateAsSource {
                migration_id,
                task: migrate_task,
                start_tx,
                command_rx,
                response_tx,
            }),
            &mut next_external,
            &mut next_lifecycle,
        );

        // Tell the state driver to pause entities, just like in a real
        // migration.
        command_tx.send(MigrateSourceCommand::Pause).await.unwrap();
        let resp = response_rx.recv().await.unwrap();
        assert!(matches!(resp, MigrateSourceResponse::Pause(Ok(()))));

        // Now tell the migration task to fail and yield an error. This should
        // produce no additional calls to resume things.
        task_exit_tx.send(Err(MigrateError::UnexpectedMessage)).unwrap();
    }
    */
}
