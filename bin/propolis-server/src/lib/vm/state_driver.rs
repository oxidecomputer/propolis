// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::sync::Arc;

use crate::migrate::MigrateError;
use crate::vcpu_tasks::VcpuTaskController;

use super::{
    request_queue, ExternalRequest, GuestEvent, MigrateSourceCommand,
    MigrateSourceResponse, MigrateTargetCommand, MigrateTaskEvent,
    SharedVmState, StateDriverEvent,
};

use propolis_api_types::{
    InstanceMigrateStatusResponse as ApiMigrationStatus,
    InstanceState as ApiInstanceState,
    InstanceStateMonitorResponse as ApiMonitoredState,
    MigrationState as ApiMigrationState,
};
use slog::{error, info, Logger};
use uuid::Uuid;

#[usdt::provider(provider = "propolis")]
mod probes {
    fn state_driver_pause() {}
    fn state_driver_resume() {}
}

/// Tells the state driver whether or not to continue running after responding
/// to an event.
#[derive(Debug, PartialEq, Eq)]
enum HandleEventOutcome {
    Continue,
    Exit,
}

#[derive(Debug, PartialEq, Eq)]
enum VmStartReason {
    MigratedIn,
    ExplicitRequest,
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
    /// requests to send messages to devices or update other VM controller
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

    /// Whether the worker's VM's devices are paused.
    paused: bool,

    /// The sender side of the monitor that reflects the instance's current
    /// externally-visible state (including migration state).
    api_state_tx: tokio::sync::watch::Sender<ApiMonitoredState>,
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
        }
    }

    /// Yields the current externally-visible instance state.
    fn get_instance_state(&self) -> ApiInstanceState {
        self.api_state_tx.borrow().state
    }

    /// Publishes the supplied externally-visible instance state to the external
    /// instance state channel.
    fn set_instance_state(&mut self, state: ApiInstanceState) {
        let old = self.api_state_tx.borrow().clone();

        self.state_gen += 1;
        let _ = self.api_state_tx.send(ApiMonitoredState {
            gen: self.state_gen,
            state,
            ..old
        });
    }

    /// Retrieves the most recently published migration state from the external
    /// migration state channel.
    ///
    /// This function does not return the borrowed monitor, so the state may
    /// change again as soon as this function returns.
    fn get_migration_status(&self) -> Option<ApiMigrationStatus> {
        self.api_state_tx.borrow().migration.clone()
    }

    /// Publishes the supplied externally-visible migration status to the
    /// instance state channel.
    fn set_migration_state(
        &mut self,
        migration_id: Uuid,
        state: ApiMigrationState,
    ) {
        let old = self.api_state_tx.borrow().clone();

        self.state_gen += 1;
        let _ = self.api_state_tx.send(ApiMonitoredState {
            gen: self.state_gen,
            migration: Some(ApiMigrationStatus { migration_id, state }),
            ..old
        });
    }

    /// Manages an instance's lifecycle once it has moved to the Running state.
    pub(super) fn run_state_worker(
        mut self,
    ) -> tokio::sync::watch::Sender<ApiMonitoredState> {
        info!(self.log, "State worker launched");

        loop {
            let event = self.shared_state.wait_for_next_event();
            info!(self.log, "State worker handling event"; "event" => ?event);

            let outcome = self.handle_event(event);
            info!(self.log, "State worker handled event"; "outcome" => ?outcome);
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
                self.migrate_as_target(
                    migration_id,
                    task,
                    start_tx,
                    command_rx,
                );
                HandleEventOutcome::Continue
            }
            ExternalRequest::Start => {
                self.start_vm(VmStartReason::ExplicitRequest);
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
                self.migrate_as_source(
                    migration_id,
                    task,
                    start_tx,
                    command_rx,
                    response_tx,
                );
                HandleEventOutcome::Continue
            }
            ExternalRequest::Stop => {
                self.do_halt();
                HandleEventOutcome::Exit
            }
        }
    }

    fn handle_guest_event(&mut self, event: GuestEvent) -> HandleEventOutcome {
        match event {
            GuestEvent::VcpuSuspendHalt(_when) => {
                info!(self.log, "Halting due to VM suspend event",);
                self.do_halt();
                HandleEventOutcome::Exit
            }
            GuestEvent::VcpuSuspendReset(_when) => {
                info!(self.log, "Resetting due to VM suspend event");
                self.do_reboot();
                HandleEventOutcome::Continue
            }
            GuestEvent::VcpuSuspendTripleFault(vcpu_id, _when) => {
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
                HandleEventOutcome::Exit
            }
            GuestEvent::ChipsetReset => {
                info!(self.log, "Resetting due to chipset-driven reset");
                self.do_reboot();
                HandleEventOutcome::Continue
            }
        }
    }

    fn start_vm(&mut self, start_reason: VmStartReason) {
        info!(self.log, "Starting instance"; "reason" => ?start_reason);

        // Only move to the Starting state if this VM is starting by explicit
        // request (as opposed to the implicit start that happens after a
        // migration in). In this case, no one has initialized vCPU state yet,
        // so explicitly initialize it here.
        //
        // In the migration-in case, remain in the Migrating state until the
        // VM is actually running. Note that this is contractual behavior--sled
        // agent relies on this to represent that a migrating instance is
        // continuously running through a successful migration.
        match start_reason {
            VmStartReason::ExplicitRequest => {
                self.set_instance_state(ApiInstanceState::Starting);
                self.reset_vcpus();
            }
            VmStartReason::MigratedIn => {
                assert_eq!(
                    self.get_instance_state(),
                    ApiInstanceState::Migrating
                );
                // Signal the kernel VMM to resume devices which are handled by
                // the in-kernel emulation.  They were kept paused for
                // consistency while migration state was loaded.
                self.controller.resume_vm();
            }
        }

        match self.controller.start_devices() {
            Ok(()) => {
                self.vcpu_tasks.resume_all();
                self.publish_steady_state(ApiInstanceState::Running);
            }
            Err(e) => {
                error!(&self.log, "Failed to start devices: {:?}", e);
                self.publish_steady_state(ApiInstanceState::Failed);
            }
        }
    }

    fn do_reboot(&mut self) {
        info!(self.log, "Resetting instance");

        self.set_instance_state(ApiInstanceState::Rebooting);

        // Reboot is implemented as a pause -> reset -> resume transition.
        //
        // First, pause the vCPUs and all devices so no partially-completed
        // work is present.
        self.vcpu_tasks.pause_all();
        self.controller.pause_devices();

        // Reset all the entities and the VM's bhyve state, then reset the
        // vCPUs. The vCPU reset must come after the bhyve reset.
        self.controller.reset_devices_and_machine();
        self.reset_vcpus();

        // Resume devices so they're ready to do more work, then resume vCPUs.
        self.controller.resume_devices();
        self.vcpu_tasks.resume_all();

        // Notify the request queue that this reboot request was processed.
        // This does not use the `publish_steady_state` path because the queue
        // treats an instance's initial transition to "Running" as a one-time
        // event that's different from a return to the running state from a
        // transient intermediate state.
        self.notify_request_queue(request_queue::InstanceStateChange::Rebooted);
        self.set_instance_state(ApiInstanceState::Running);
    }

    fn do_halt(&mut self) {
        info!(self.log, "Stopping instance");
        self.set_instance_state(ApiInstanceState::Stopping);

        // Entities expect to be paused before being halted. Note that the VM
        // may be paused already if it is being torn down after a successful
        // migration out.
        if !self.paused {
            self.pause();
        }

        self.vcpu_tasks.exit_all();
        self.controller.halt_devices();
        self.publish_steady_state(ApiInstanceState::Stopped);
    }

    fn migrate_as_target(
        &mut self,
        migration_id: Uuid,
        mut task: tokio::task::JoinHandle<Result<(), MigrateError>>,
        start_tx: tokio::sync::oneshot::Sender<()>,
        mut command_rx: tokio::sync::mpsc::Receiver<MigrateTargetCommand>,
    ) {
        self.set_instance_state(ApiInstanceState::Migrating);

        // Ensure the VM's vCPUs are activated properly so that they can enter
        // the guest after migration. Do this before allowing the migration task
        // to start so that reset doesn't overwrite any state written by
        // migration.
        self.reset_vcpus();

        // Place the VM in a paused state so we can load emulated device state
        // in a consistent manner
        self.controller.pause_vm();

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
                    if res.is_ok() {
                        // Clients that observe that migration has finished
                        // need to observe that the instance is running before
                        // they are guaranteed to be able to do anything else
                        // that requires a running instance.
                        assert!(matches!(
                            self.get_migration_status().unwrap().state,
                            ApiMigrationState::Finish
                        ));

                        self.start_vm(VmStartReason::MigratedIn);
                    } else {
                        assert!(matches!(
                            self.get_migration_status().unwrap().state,
                            ApiMigrationState::Error
                        ));

                        // Resume the kernel VM so that if this state driver is
                        // asked to halt, the pause resulting therefrom won't
                        // observe that the VM is already paused.
                        self.controller.resume_vm();
                        self.publish_steady_state(ApiInstanceState::Failed);
                    };

                    break;
                }
                MigrateTaskEvent::Command(
                    MigrateTargetCommand::UpdateState(state),
                ) => {
                    self.set_migration_state(migration_id, state);
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
    ) {
        self.set_instance_state(ApiInstanceState::Migrating);
        start_tx.send(()).unwrap();

        // Wait either for the migration task to exit or for it to ask the
        // worker to pause or resume the instance's devices.
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
                // If migration failed while devices were paused, this instance
                // is allowed to resume, so resume its components here.
                MigrateTaskEvent::TaskExited(res) => {
                    if res.is_ok() {
                        assert!(matches!(
                            self.get_migration_status().unwrap().state,
                            ApiMigrationState::Finish
                        ));

                        self.shared_state
                            .queue_external_request(ExternalRequest::Stop)
                            .expect("can always queue a request to stop");
                    } else {
                        assert!(matches!(
                            self.get_migration_status().unwrap().state,
                            ApiMigrationState::Error
                        ));

                        if self.paused {
                            self.resume();
                            self.publish_steady_state(
                                ApiInstanceState::Running,
                            );
                        }
                    }

                    break;
                }
                MigrateTaskEvent::Command(cmd) => match cmd {
                    MigrateSourceCommand::UpdateState(state) => {
                        self.set_migration_state(migration_id, state);
                    }
                    MigrateSourceCommand::Pause => {
                        self.pause();
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

    fn pause(&mut self) {
        assert!(!self.paused);
        probes::state_driver_pause!(|| ());
        self.vcpu_tasks.pause_all();
        self.controller.pause_devices();
        self.controller.pause_vm();
        self.paused = true;
    }

    fn resume(&mut self) {
        assert!(self.paused);
        probes::state_driver_resume!(|| ());
        self.controller.resume_vm();
        self.controller.resume_devices();
        self.vcpu_tasks.resume_all();
        self.paused = false;
    }

    fn reset_vcpus(&self) {
        self.vcpu_tasks.new_generation();
        self.controller.reset_vcpu_state();
    }

    fn publish_steady_state(&mut self, state: ApiInstanceState) {
        let change = match state {
            ApiInstanceState::Running => {
                request_queue::InstanceStateChange::StartedRunning
            }
            ApiInstanceState::Stopped => {
                request_queue::InstanceStateChange::Stopped
            }
            ApiInstanceState::Failed => {
                request_queue::InstanceStateChange::Failed
            }
            _ => panic!(
                "Called publish_steady_state on non-terminal state {:?}",
                state
            ),
        };

        self.notify_request_queue(change);
        self.set_instance_state(state);
    }

    fn notify_request_queue(
        &self,
        queue_change: request_queue::InstanceStateChange,
    ) {
        self.shared_state
            .inner
            .lock()
            .unwrap()
            .external_request_queue
            .notify_instance_state_change(queue_change);
    }
}

#[cfg(test)]
mod tests {
    use anyhow::bail;
    use mockall::Sequence;

    use super::*;
    use crate::vcpu_tasks::MockVcpuTaskController;
    use crate::vm::MockStateDriverVmController;

    struct TestStateDriver {
        driver:
            StateDriver<MockStateDriverVmController, MockVcpuTaskController>,
        state_rx: tokio::sync::watch::Receiver<ApiMonitoredState>,
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
                migration: None,
            });

        TestStateDriver {
            driver: StateDriver::new(
                tokio::runtime::Handle::current(),
                Arc::new(objects.vm_ctrl),
                objects.shared_state.clone(),
                objects.vcpu_ctrl,
                logger,
                state_tx,
            ),
            state_rx,
        }
    }

    /// Generates default mocks for the VM controller and vCPU task controller
    /// that accept unlimited requests to read state.
    fn make_default_mocks() -> TestObjects {
        let logger = slog::Logger::root(slog::Discard, slog::o!());
        let vm_ctrl = MockStateDriverVmController::new();
        let vcpu_ctrl = MockVcpuTaskController::new();
        TestObjects {
            vm_ctrl,
            vcpu_ctrl,
            shared_state: Arc::new(SharedVmState::new(&logger)),
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
        // whether vCPUs or devices pause first, but there's no way to specify
        // that these events must be sequenced before other expectations but
        // have no ordering with respect to each other.
        vcpu_ctrl
            .expect_pause_all()
            .times(1)
            .in_sequence(&mut seq)
            .returning(|| ());
        vm_ctrl
            .expect_pause_devices()
            .times(1)
            .in_sequence(&mut seq)
            .returning(|| ());

        // The devices and--importantly--the bhyve VM itself must be reset
        // before resetting any vCPU state (so that bhyve will accept the ioctls
        // sent to the vCPUs during the reset process).
        vm_ctrl
            .expect_reset_devices_and_machine()
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
        // resuming devices first allows them to be ready when the vCPUs start
        // creating work for them to do.
        vm_ctrl
            .expect_resume_devices()
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
            GuestEvent::VcpuSuspendTripleFault(
                0,
                std::time::Duration::default(),
            ),
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
            .expect_start_devices()
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
    }

    #[tokio::test]
    async fn device_start_failure_causes_instance_failure() {
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
            .expect_start_devices()
            .times(1)
            .in_sequence(&mut seq)
            .returning(|| bail!("injected failure into start_devices!"));

        let mut driver = make_state_driver(test_objects);

        // Failure allows the instance to be preserved for debugging.
        assert_eq!(
            driver.driver.handle_event(StateDriverEvent::External(
                ExternalRequest::Start
            )),
            HandleEventOutcome::Continue
        );

        assert!(matches!(driver.api_state(), ApiInstanceState::Failed));
    }

    #[tokio::test]
    async fn devices_pause_before_halting() {
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
            .expect_pause_devices()
            .times(1)
            .in_sequence(&mut seq)
            .returning(|| ());
        vm_ctrl
            .expect_pause_vm()
            .times(1)
            .in_sequence(&mut seq)
            .returning(|| ());
        vcpu_ctrl
            .expect_exit_all()
            .times(1)
            .in_sequence(&mut seq)
            .returning(|| ());
        vm_ctrl
            .expect_halt_devices()
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
    async fn devices_pause_once_when_halting_after_migration_out() {
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
        vm_ctrl.expect_pause_devices().times(1).returning(|| ());
        vcpu_ctrl.expect_pause_all().times(1).returning(|| ());
        vcpu_ctrl.expect_exit_all().times(1).returning(|| ());
        vm_ctrl.expect_halt_devices().times(1).returning(|| ());
        vm_ctrl.expect_resume_devices().never();
        vcpu_ctrl.expect_resume_all().never();
        vm_ctrl.expect_pause_vm().times(1).returning(|| ());
        vm_ctrl.expect_resume_vm().never();

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

        // The migration should appear to have finished. The state driver will
        // queue a "stop" command to itself in this case, but because the driver
        // is not directly processing the queue here, the test has to issue this
        // call itself.
        assert_eq!(
            driver.driver.get_migration_status().unwrap(),
            ApiMigrationStatus {
                migration_id,
                state: ApiMigrationState::Finish
            }
        );

        driver
            .driver
            .handle_event(StateDriverEvent::External(ExternalRequest::Stop));

        assert!(matches!(driver.api_state(), ApiInstanceState::Stopped));
    }

    #[tokio::test]
    async fn paused_vm_resumes_after_failed_migration_out() {
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

        // This test will simulate a migration out up through pausing the
        // source, then fail migration. This should pause and resume all the
        // devices and the vCPUs.
        vm_ctrl.expect_pause_devices().times(1).returning(|| ());
        vm_ctrl.expect_resume_devices().times(1).returning(|| ());
        vcpu_ctrl.expect_pause_all().times(1).returning(|| ());
        vcpu_ctrl.expect_resume_all().times(1).returning(|| ());

        // VMM will be paused once prior to exporting state, and then resumed
        // afterwards when the migration fails.
        let mut pause_seq = Sequence::new();
        vm_ctrl
            .expect_pause_vm()
            .times(1)
            .in_sequence(&mut pause_seq)
            .returning(|| ());
        vm_ctrl
            .expect_resume_vm()
            .times(1)
            .in_sequence(&mut pause_seq)
            .returning(|| ());

        let mut driver = make_state_driver(test_objects);
        let hdl = std::thread::spawn(move || {
            let outcome = driver.driver.handle_event(
                StateDriverEvent::External(ExternalRequest::MigrateAsSource {
                    migration_id,
                    task: migrate_task,
                    start_tx,
                    command_rx,
                    response_tx,
                }),
            );

            (driver, outcome)
        });

        // Simulate a successful pause.
        command_tx.send(MigrateSourceCommand::Pause).await.unwrap();
        let resp = response_rx.recv().await.unwrap();
        assert!(matches!(resp, MigrateSourceResponse::Pause(Ok(()))));

        // Simulate failure. The migration protocol must both update the state
        // to Error and make the task return `Err`.
        command_tx
            .send(MigrateSourceCommand::UpdateState(ApiMigrationState::Error))
            .await
            .unwrap();
        drop(command_tx);
        task_exit_tx.send(Err(MigrateError::UnexpectedMessage)).unwrap();

        // Wait for the call to `handle_event` to return.
        let (driver, outcome) =
            tokio::task::spawn_blocking(move || hdl.join().unwrap())
                .await
                .unwrap();

        // The VM should be running and the state driver should continue
        // operating normally.
        assert!(matches!(driver.api_state(), ApiInstanceState::Running));
        assert_eq!(outcome, HandleEventOutcome::Continue);
        assert_eq!(
            driver.driver.get_migration_status().unwrap(),
            ApiMigrationStatus {
                migration_id,
                state: ApiMigrationState::Error
            }
        );
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
        vm_ctrl.expect_start_devices().times(1).returning(|| Ok(()));
        vcpu_ctrl.expect_resume_all().times(1).returning(|| ());

        let mut pause_seq = Sequence::new();
        vm_ctrl
            .expect_pause_vm()
            .times(1)
            .in_sequence(&mut pause_seq)
            .returning(|| ());
        vm_ctrl
            .expect_resume_vm()
            .times(1)
            .in_sequence(&mut pause_seq)
            .returning(|| ());

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

        assert_eq!(
            driver.driver.get_migration_status().unwrap(),
            ApiMigrationStatus {
                migration_id,
                state: ApiMigrationState::Finish
            }
        );
        assert!(matches!(driver.api_state(), ApiInstanceState::Running));
    }

    #[tokio::test]
    async fn failed_migration_in_fails_instance() {
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
        vm_ctrl.expect_pause_vm().times(1).returning(|| ());
        vm_ctrl.expect_resume_vm().times(1).returning(|| ());
        let mut driver = make_state_driver(test_objects);

        // The state driver expects to run on an OS thread outside the async
        // runtime so that it can call `block_on` to wait for messages from the
        // migration task.
        let hdl = std::thread::spawn(move || {
            let outcome = driver.driver.handle_event(
                StateDriverEvent::External(ExternalRequest::MigrateAsTarget {
                    migration_id,
                    task: migrate_task,
                    start_tx,
                    command_rx,
                }),
            );

            (driver, outcome)
        });

        // The migration task is required to update the migration state to
        // "Error" before exiting when migration fails.
        command_tx
            .send(MigrateTargetCommand::UpdateState(ApiMigrationState::Error))
            .await
            .unwrap();
        drop(command_tx);
        task_exit_tx.send(Err(MigrateError::UnexpectedMessage)).unwrap();

        // Wait for the call to `handle_event` to return.
        let (driver, outcome) =
            tokio::task::spawn_blocking(move || hdl.join().unwrap())
                .await
                .unwrap();

        // The migration should appear to have failed, but the VM should be
        // preserved for debugging.
        assert_eq!(outcome, HandleEventOutcome::Continue);
        assert!(matches!(driver.api_state(), ApiInstanceState::Failed));
        assert_eq!(
            driver.driver.get_migration_status().unwrap(),
            ApiMigrationStatus {
                migration_id,
                state: ApiMigrationState::Error
            }
        );
    }

    #[tokio::test]
    async fn failed_vm_start_after_migration_in_fails_instance() {
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

        let mut pause_seq = Sequence::new();
        vm_ctrl
            .expect_pause_vm()
            .times(1)
            .in_sequence(&mut pause_seq)
            .returning(|| ());
        vm_ctrl
            .expect_resume_vm()
            .times(1)
            .in_sequence(&mut pause_seq)
            .returning(|| ());

        vm_ctrl
            .expect_start_devices()
            .times(1)
            .returning(|| bail!("injected failure into start_devices!"));

        let mut driver = make_state_driver(test_objects);

        // The state driver expects to run on an OS thread outside the async
        // runtime so that it can call `block_on` to wait for messages from the
        // migration task.
        let hdl = std::thread::spawn(move || {
            let outcome = driver.driver.handle_event(
                StateDriverEvent::External(ExternalRequest::MigrateAsTarget {
                    migration_id,
                    task: migrate_task,
                    start_tx,
                    command_rx,
                }),
            );

            (driver, outcome)
        });

        // Explicitly drop the command channel to signal to the driver that
        // the migration task is completing.
        command_tx
            .send(MigrateTargetCommand::UpdateState(ApiMigrationState::Finish))
            .await
            .unwrap();
        drop(command_tx);
        task_exit_tx.send(Ok(())).unwrap();

        // Wait for the call to `handle_event` to return.
        let (driver, outcome) =
            tokio::task::spawn_blocking(move || hdl.join().unwrap())
                .await
                .unwrap();

        // The instance should have failed, but should also be preserved for
        // debugging.
        assert_eq!(outcome, HandleEventOutcome::Continue);
        assert!(matches!(driver.api_state(), ApiInstanceState::Failed));

        // The migration has still succeeded in this case.
        assert_eq!(
            driver.driver.get_migration_status().unwrap(),
            ApiMigrationStatus {
                migration_id,
                state: ApiMigrationState::Finish
            }
        );
    }

    #[tokio::test]
    async fn start_vm_after_migration_in_does_not_publish_starting_state() {
        let mut test_objects = make_default_mocks();
        let vm_ctrl = &mut test_objects.vm_ctrl;
        let vcpu_ctrl = &mut test_objects.vcpu_ctrl;

        // A call to start a VM after a successful migration should start vCPUs
        // and devices without resetting anything.
        vcpu_ctrl.expect_resume_all().times(1).returning(|| ());
        vm_ctrl.expect_start_devices().times(1).returning(|| Ok(()));

        // As noted below, the instance state is being magicked directly into a
        // `Migrating` state, rather than executing the logic which would
        // typically carry it there.  As such, `pause_vm()` will not be called
        // as part of setup.  Since instance start _is_ being tested here, the
        // `resume_vm()` call is expected.
        vm_ctrl.expect_pause_vm().never();
        vm_ctrl.expect_resume_vm().times(1).returning(|| ());

        // Skip the rigmarole of standing up a fake migration. Instead, just
        // push the driver into the state it would have after a successful
        // migration to appease the assertions in `start_vm`.
        //
        // Faking an entire migration, as in the previous tests, requires the
        // state driver to run on its own worker thread. This is fine for tests
        // that only want to examine state after the driver has finished an
        // operation, but this test wants to test side effects of a specific
        // part of the state driver's actions, which are tough to synchronize
        // with when the driver is running on another thread.
        let mut driver = make_state_driver(test_objects);
        driver.driver.set_instance_state(ApiInstanceState::Migrating);

        // The driver starts in the Migrating state and should go directly to
        // the Running state without passing through Starting. Because there's
        // no way to guarantee that the test will see all intermediate states
        // that `start_vm` publishes, instead assert that the final state of
        // Running is correct and that the state generation only went up by 1
        // (implying that there were no intervening transitions).
        let migrating_gen = driver.driver.api_state_tx.borrow().gen;
        driver.driver.start_vm(VmStartReason::MigratedIn);
        let new_state = driver.driver.api_state_tx.borrow().clone();
        assert!(matches!(new_state.state, ApiInstanceState::Running));
        assert_eq!(new_state.gen, migrating_gen + 1);
    }
}
