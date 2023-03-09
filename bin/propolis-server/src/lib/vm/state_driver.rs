use std::sync::Arc;

use crate::migrate::MigrateError;

use super::{
    ExternalRequest, GuestEvent, MigrateSourceCommand, MigrateSourceResponse,
    MigrateTargetCommand, MigrateTaskEvent, SharedVmState, StateDriverEvent,
};

use propolis_client::handmade::{
    api::InstanceState as ApiInstanceState,
    api::InstanceStateMonitorResponse as ApiMonitoredState,
    api::MigrationState as ApiMigrationState,
};
use slog::{info, Logger};
use uuid::Uuid;

use crate::vcpu_tasks::VcpuTaskController;

/// Tells the state driver whether or not to continue running after responding
/// to an event.
enum HandleEventOutcome {
    Continue,
    Exit,
}

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

    /// A reference to the state this driver shares with its VM controller.
    shared_state: Arc<SharedVmState>,

    /// The controller for this instance's vCPU tasks.
    vcpu_tasks: C,

    /// The state worker's logger.
    log: Logger,

    /// The generation number to use when publishing externally-visible state
    /// updates.
    state_gen: u64,

    /// Whether the worker's VM's entities are paused.
    paused: bool,

    /// The sender side of the monitor that reflects the instance's current
    /// externally-visible state.
    api_state_tx: tokio::sync::watch::Sender<ApiMonitoredState>,

    /// The sender side of the monitor that reflects the state of the current
    /// or most recent active migration into or out of this instance.
    migration_state_tx:
        tokio::sync::watch::Sender<Option<(Uuid, ApiMigrationState)>>,
}

impl<V, C> StateDriver<V, C>
where
    V: super::StateDriverVmController,
    C: VcpuTaskController,
{
    /// Constructs a new state driver context.
    pub(super) fn new(
        runtime_hdl: tokio::runtime::Handle,
        controller: Arc<V>,
        shared_controller_state: Arc<SharedVmState>,
        vcpu_tasks: C,
        log: Logger,
        api_state_tx: tokio::sync::watch::Sender<ApiMonitoredState>,
        migration_state_tx: tokio::sync::watch::Sender<
            Option<(Uuid, ApiMigrationState)>,
        >,
    ) -> Self {
        Self {
            runtime_hdl,
            controller,
            shared_state: shared_controller_state,
            vcpu_tasks,
            log,
            state_gen: 0,
            paused: false,
            api_state_tx,
            migration_state_tx,
        }
    }

    /// Publishes the supplied externally-visible instance state to the external
    /// instance state channel.
    fn update_external_state(&mut self, state: ApiInstanceState) {
        self.state_gen += 1;
        let _ = self
            .api_state_tx
            .send(ApiMonitoredState { gen: self.state_gen, state });
    }

    /// Retrieves the most recently published migration state from the external
    /// migration state channel.
    ///
    /// This function does not return the borrowed monitor, so the state may
    /// change again as soon as this function returns.
    fn get_migration_state(&self) -> Option<(Uuid, ApiMigrationState)> {
        *self.migration_state_tx.borrow()
    }

    /// Publishes the supplied externally-visible migration status to the
    /// external migration state channel.
    fn set_migration_state(
        &mut self,
        migration_id: Uuid,
        state: ApiMigrationState,
    ) {
        let _ = self.migration_state_tx.send(Some((migration_id, state)));
    }

    /// Manages an instance's lifecycle once it has moved to the Running state.
    pub(super) fn run_state_worker(
        mut self,
    ) -> tokio::sync::watch::Sender<ApiMonitoredState> {
        info!(self.log, "State worker launched");

        loop {
            let event = self.shared_state.wait_for_next_event();
            let outcome = self.handle_event(event);
            if matches!(outcome, HandleEventOutcome::Exit) {
                break;
            }
        }

        info!(self.log, "State worker exiting");

        self.api_state_tx
    }

    fn handle_event(&mut self, event: StateDriverEvent) -> HandleEventOutcome {
        let next_action = match event {
            StateDriverEvent::Guest(guest_event) => {
                return self.handle_guest_event(guest_event);
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
                self.update_external_state(ApiInstanceState::Migrating);

                // Ensure the vCPUs are activated properly so that they can
                // enter the guest after migration. Do this before launching
                // the migration task so that reset doesn't overwrite the
                // state written by migration.
                self.reset_vcpus();
                match self.migrate_as_target(
                    migration_id,
                    task,
                    start_tx,
                    command_rx,
                ) {
                    Ok(_) => {
                        // TODO(gjc) update allowed external requests
                        self.set_migration_state(
                            migration_id,
                            ApiMigrationState::Finish,
                        );

                        // Resume the VM immediately without waiting for a
                        // request to enter the "running" state.
                        //
                        // TODO(#209) Transition to a "failed" state instead of
                        // expecting.
                        self.start_vm(false)
                            .expect("failed to start VM after migrating");
                        HandleEventOutcome::Continue
                    }
                    Err(_) => HandleEventOutcome::Continue,
                }
            }
            ExternalRequest::Start => {
                // TODO(#209) Transition to a "failed" state instead of
                // expecting.
                self.start_vm(true).expect("failed to start VM");
                // TODO(gjc) update allowed requests
                HandleEventOutcome::Continue
            }
            ExternalRequest::Reboot => {
                self.do_reboot();
                HandleEventOutcome::Continue
            }
            ExternalRequest::MigrateAsSource {
                migration_id,
                task,
                start_tx,
                command_rx,
                response_tx,
            } => {
                self.update_external_state(ApiInstanceState::Migrating);
                let _result = self.migrate_as_source(
                    migration_id,
                    task,
                    start_tx,
                    command_rx,
                    response_tx,
                );
                // TODO(gjc) update allowed requests
                HandleEventOutcome::Continue
            }
            ExternalRequest::Stop => {
                self.do_halt();
                // TODO(gjc) update allowed requests
                HandleEventOutcome::Exit
            }
        }
    }

    fn handle_guest_event(&mut self, event: GuestEvent) -> HandleEventOutcome {
        match event {
            GuestEvent::VcpuSuspendHalt(vcpu_id) => {
                info!(
                    self.log,
                    "Halting due to halt event on vCPU {}", vcpu_id
                );
                self.do_halt();
                // TODO(gjc) update allowed requests
                HandleEventOutcome::Exit
            }
            GuestEvent::VcpuSuspendReset(vcpu_id) => {
                info!(
                    self.log,
                    "Resetting due to reset event on vCPU {}", vcpu_id
                );
                self.do_reboot();
                HandleEventOutcome::Continue
            }
            GuestEvent::VcpuSuspendTripleFault(vcpu_id) => {
                info!(
                    self.log,
                    "Resetting due to triple fault on vCPU {}", vcpu_id
                );
                self.do_reboot();
                HandleEventOutcome::Continue
            }
            GuestEvent::ChipsetHalt => {
                info!(self.log, "Halting due to chipset-driven halt");
                self.do_halt();
                // TODO(gjc) update allowed requests
                HandleEventOutcome::Exit
            }
            GuestEvent::ChipsetReset => {
                info!(self.log, "Resetting due to chipset-driven reset");
                self.do_reboot();
                HandleEventOutcome::Continue
            }
        }
    }

    fn start_vm(&mut self, reset_required: bool) -> anyhow::Result<()> {
        info!(self.log, "Starting instance"; "reset" => reset_required);

        // Requests to start should put the instance into the 'starting' stage.
        // Reflect this out to callers who ask what state the VM is in.
        self.update_external_state(ApiInstanceState::Starting);

        if reset_required {
            self.reset_vcpus();
        }

        self.controller.start_entities()?;
        self.vcpu_tasks.resume_all();
        self.update_external_state(ApiInstanceState::Running);
        Ok(())
    }

    fn do_reboot(&mut self) {
        info!(self.log, "Resetting instance");

        self.update_external_state(ApiInstanceState::Rebooting);

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

        self.update_external_state(ApiInstanceState::Running);
    }

    fn do_halt(&mut self) {
        info!(self.log, "Stopping instance");
        self.update_external_state(ApiInstanceState::Stopping);

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
        self.update_external_state(ApiInstanceState::Stopped);
    }

    fn migrate_as_target(
        &mut self,
        migration_id: Uuid,
        mut task: tokio::task::JoinHandle<Result<(), MigrateError>>,
        start_tx: tokio::sync::oneshot::Sender<()>,
        mut command_rx: tokio::sync::mpsc::Receiver<MigrateTargetCommand>,
    ) -> Result<(), MigrateError> {
        start_tx.send(()).unwrap();
        let mut finished = false;
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
                    // If the task finished successfully but didn't signal that
                    // it finished its work, something is amiss.
                    assert_eq!(finished, res.is_ok());
                    return res;
                }
                MigrateTaskEvent::Command(
                    MigrateTargetCommand::UpdateState(state),
                ) => {
                    // The "finished" state must always arrive last.
                    assert!(!finished);

                    // Do not publish the "finished" state from this point;
                    // instead, let the migration complete and return success to
                    // the caller so that it can decide when to publish this
                    // state relative to other changes to the VM's lifecycle
                    // stage.
                    if matches!(state, ApiMigrationState::Finish) {
                        finished = true;
                    } else {
                        self.set_migration_state(migration_id, state);
                    }
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
                            self.set_migration_state(
                                migration_id,
                                ApiMigrationState::Error,
                            );
                        }
                    } else {
                        assert!(matches!(
                            self.get_migration_state(),
                            Some((_, ApiMigrationState::Finish))
                        ));
                    }
                    return res;
                }
                MigrateTaskEvent::Command(cmd) => match cmd {
                    MigrateSourceCommand::UpdateState(state) => {
                        self.set_migration_state(migration_id, state);
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

    struct TestStateDriver {
        driver:
            StateDriver<MockStateDriverVmController, MockVcpuTaskController>,
        state_rx: tokio::sync::watch::Receiver<ApiMonitoredState>,
        _migrate_rx:
            tokio::sync::watch::Receiver<Option<(Uuid, ApiMigrationState)>>,
    }

    impl TestStateDriver {
        fn api_state(&self) -> ApiInstanceState {
            self.state_rx.borrow().state
        }
    }

    struct TestObjects {
        vm_ctrl: MockStateDriverVmController,
        vcpu_ctrl: MockVcpuTaskController,
        shared_state: Arc<SharedVmState>,
    }

    fn make_state_driver(objects: TestObjects) -> TestStateDriver {
        let logger = slog::Logger::root(slog::Discard, slog::o!());
        let (state_tx, state_rx) =
            tokio::sync::watch::channel(ApiMonitoredState {
                gen: 0,
                state: ApiInstanceState::Creating,
            });
        let (migrate_tx, migrate_rx) = tokio::sync::watch::channel(None);

        TestStateDriver {
            driver: StateDriver::new(
                tokio::runtime::Handle::current(),
                Arc::new(objects.vm_ctrl),
                objects.shared_state.clone(),
                objects.vcpu_ctrl,
                logger,
                state_tx,
                migrate_tx,
            ),
            state_rx,
            _migrate_rx: migrate_rx,
        }
    }

    /// Generates default mocks for the VM controller and vCPU task controller
    /// that accept unlimited requests to read state.
    fn make_default_mocks() -> TestObjects {
        let vm_ctrl = MockStateDriverVmController::new();
        let vcpu_ctrl = MockVcpuTaskController::new();
        TestObjects {
            vm_ctrl,
            vcpu_ctrl,
            shared_state: Arc::new(SharedVmState::new()),
        }
    }

    fn add_reboot_expectations(
        vm_ctrl: &mut MockStateDriverVmController,
        vcpu_ctrl: &mut MockVcpuTaskController,
    ) {
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
        let mut test_objects = make_default_mocks();

        add_reboot_expectations(
            &mut test_objects.vm_ctrl,
            &mut test_objects.vcpu_ctrl,
        );
        let mut driver = make_state_driver(test_objects);
        driver.driver.handle_event(StateDriverEvent::Guest(
            GuestEvent::VcpuSuspendTripleFault(0),
        ));

        assert!(matches!(driver.api_state(), ApiInstanceState::Running));
    }

    #[tokio::test]
    async fn guest_chipset_reset_reboots() {
        let mut test_objects = make_default_mocks();

        add_reboot_expectations(
            &mut test_objects.vm_ctrl,
            &mut test_objects.vcpu_ctrl,
        );
        let mut driver = make_state_driver(test_objects);
        driver
            .driver
            .handle_event(StateDriverEvent::Guest(GuestEvent::ChipsetReset));

        assert!(matches!(driver.api_state(), ApiInstanceState::Running));
    }

    #[tokio::test]
    async fn start_from_cold_boot() {
        let mut test_objects = make_default_mocks();
        let vm_ctrl = &mut test_objects.vm_ctrl;
        let vcpu_ctrl = &mut test_objects.vcpu_ctrl;
        let mut seq = Sequence::new();
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

        let mut driver = make_state_driver(test_objects);
        driver
            .driver
            .handle_event(StateDriverEvent::External(ExternalRequest::Start));

        assert!(matches!(driver.api_state(), ApiInstanceState::Running));
        // TODO(gjc) assert!(matches!(next_lifecycle, Some(LifecycleStage::Active)));
    }

    #[tokio::test]
    async fn entities_pause_before_halting() {
        let mut test_objects = make_default_mocks();
        let vm_ctrl = &mut test_objects.vm_ctrl;
        let vcpu_ctrl = &mut test_objects.vcpu_ctrl;
        let mut seq = Sequence::new();
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

        let mut driver = make_state_driver(test_objects);
        driver
            .driver
            .handle_event(StateDriverEvent::External(ExternalRequest::Stop));

        assert!(matches!(driver.api_state(), ApiInstanceState::Stopped));
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

        let mut test_objects = make_default_mocks();
        let vm_ctrl = &mut test_objects.vm_ctrl;
        let vcpu_ctrl = &mut test_objects.vcpu_ctrl;

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

        let mut driver = make_state_driver(test_objects);

        // The state driver expects to run on an OS thread outside the async
        // runtime so that it can call `block_on` to wait for messages from the
        // migration task.
        let hdl = std::thread::spawn(move || {
            driver.driver.handle_event(StateDriverEvent::External(
                ExternalRequest::MigrateAsSource {
                    migration_id,
                    task: migrate_task,
                    start_tx,
                    command_rx,
                    response_tx,
                },
            ));

            // Return the driver (which has the mocks attached) when the thread
            // is joined so the test can continue using it.
            driver
        });

        // Simulate a pause and the successful completion of migration.
        command_tx.send(MigrateSourceCommand::Pause).await.unwrap();
        let resp = response_rx.recv().await.unwrap();
        assert!(matches!(resp, MigrateSourceResponse::Pause(Ok(()))));
        command_tx
            .send(MigrateSourceCommand::UpdateState(ApiMigrationState::Finish))
            .await
            .unwrap();

        drop(command_tx);
        task_exit_tx.send(Ok(())).unwrap();

        // Wait for the call to `handle_event` to return before tearing anything
        // else down.
        driver = tokio::task::spawn_blocking(move || hdl.join().unwrap())
            .await
            .unwrap();

        driver
            .driver
            .handle_event(StateDriverEvent::External(ExternalRequest::Stop));

        assert!(matches!(driver.api_state(), ApiInstanceState::Stopped));
    }

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

        let mut test_objects = make_default_mocks();
        let vm_ctrl = &mut test_objects.vm_ctrl;
        let vcpu_ctrl = &mut test_objects.vcpu_ctrl;

        vcpu_ctrl.expect_new_generation().times(1).returning(|| ());
        vm_ctrl.expect_reset_vcpu_state().times(1).returning(|| ());
        vm_ctrl.expect_start_entities().times(1).returning(|| Ok(()));
        vcpu_ctrl.expect_resume_all().times(1).returning(|| ());

        let mut driver = make_state_driver(test_objects);

        // The state driver expects to run on an OS thread outside the async
        // runtime so that it can call `block_on` to wait for messages from the
        // migration task.
        let hdl = std::thread::spawn(move || {
            driver.driver.handle_event(StateDriverEvent::External(
                ExternalRequest::MigrateAsTarget {
                    migration_id,
                    task: migrate_task,
                    start_tx,
                    command_rx,
                },
            ));

            driver
        });

        // Explicitly drop the command channel to signal to the driver that
        // the migration task is completing.
        command_tx
            .send(MigrateTargetCommand::UpdateState(ApiMigrationState::Finish))
            .await
            .unwrap();
        drop(command_tx);
        task_exit_tx.send(Ok(())).unwrap();

        // Wait for the call to `handle_event` to return before tearing anything
        // else down.
        let driver = tokio::task::spawn_blocking(move || hdl.join().unwrap())
            .await
            .unwrap();

        assert!(matches!(driver.api_state(), ApiInstanceState::Running));
        // TODO(gjc) check allowed actions
    }
}
