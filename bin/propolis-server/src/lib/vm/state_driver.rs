use std::sync::Arc;

use crate::migrate::MigrateError;

use super::{
    ExternalRequest, GuestEvent, LifecycleStage, MigrateSourceCommand,
    MigrateSourceResponse, MigrateTargetCommand, MigrateTaskEvent,
    SharedVmStateInner, StartupStage, VmController,
};

use propolis_client::handmade::{
    api::InstanceState as ApiInstanceState,
    api::MigrationState as ApiMigrationState,
};
use slog::{error, info, Logger};
use uuid::Uuid;

pub(super) struct StateDriver {
    /// A handle to the host server's tokio runtime, useful for spawning tasks
    /// that need to interact with async code (e.g. spinning up migration
    /// tasks).
    runtime_hdl: tokio::runtime::Handle,

    vcpu_tasks: crate::vcpu_tasks::VcpuTasks,

    /// The state worker's logger.
    log: Logger,

    /// The generation number to use when publishing externally-visible state
    /// updates.
    state_gen: u64,

    /// Whether the worker's VM's entities are paused.
    paused: bool,
}

impl StateDriver {
    pub(super) fn new(
        runtime_hdl: tokio::runtime::Handle,
        vcpu_tasks: crate::vcpu_tasks::VcpuTasks,
        log: Logger,
        state_gen: u64,
    ) -> Self {
        Self { runtime_hdl, vcpu_tasks, log, state_gen, paused: false }
    }

    /// Manages an instance's lifecycle once it has moved to the Running state.
    pub(super) fn run_state_worker(&mut self, controller: Arc<VmController>) {
        info!(self.log, "State worker launched");

        // Allow actions to queue up state changes that are applied on the next
        // loop iteration.
        let mut next_external: Option<ApiInstanceState> = None;
        let mut next_lifecycle: Option<LifecycleStage> = None;
        loop {
            let mut inner = controller.worker_state.inner.lock().unwrap();
            if let Some(next_lifecycle) = next_lifecycle.take() {
                inner.lifecycle_stage = next_lifecycle;
            }

            // Update the state visible to external threads querying the state
            // of this instance. If the instance is now logically stopped, this
            // thread must exit (in part to drop its reference to its VM
            // controller).
            //
            // N.B. This check must be last, because it breaks out of the loop.
            if let Some(next_external) = next_external.take() {
                inner.update_external_state(&mut self.state_gen, next_external);
                if matches!(next_external, ApiInstanceState::Stopped) {
                    break;
                }
            }

            // Wait for some work to do.
            inner = controller
                .worker_state
                .cv
                .wait_while(inner, |i| {
                    i.external_request_queue.is_empty()
                        && i.guest_event_queue.is_empty()
                })
                .unwrap();

            // The guest event queue indicates conditions that have already been
            // raised by guest entities and that need unconditional responses.
            // Handle these first.
            if let Some(event) = inner.guest_event_queue.pop_front() {
                (next_external, next_lifecycle) =
                    self.handle_guest_event(event, inner, controller.as_ref());
                continue;
            }

            let next_action = inner.external_request_queue.pop_front().unwrap();
            match next_action {
                ExternalRequest::MigrateAsTarget {
                    migration_id,
                    upgrade_fut,
                } => {
                    inner.update_external_state(
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
                        inner.lifecycle_stage,
                        LifecycleStage::NotStarted(
                            StartupStage::MigratePending
                        )
                    ));

                    drop(inner);

                    // Ensure the vCPUs are activated properly so that they can
                    // enter the guest after migration. Do this before launching
                    // the migration task so that reset doesn't overwrite the
                    // state written by migration.
                    self.reset_vcpus(&controller);
                    next_lifecycle = Some(
                        match self.migrate_as_target(
                            &controller,
                            migration_id,
                            upgrade_fut,
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
                    inner.update_external_state(
                        &mut self.state_gen,
                        ApiInstanceState::Starting,
                    );
                    next_external = Some(ApiInstanceState::Running);
                    next_lifecycle = Some(LifecycleStage::Active);
                    drop(inner);

                    if reset_required {
                        self.reset_vcpus(&controller);
                    }

                    // TODO(#209) Transition to a "failed" state here instead of
                    // expecting.
                    self.start_entities(&controller)
                        .expect("entity start callout failed");
                    self.vcpu_tasks.resume_all();
                }
                ExternalRequest::Reboot => {
                    next_external = self.do_reboot(inner, controller.as_ref());
                }
                ExternalRequest::MigrateAsSource {
                    migration_id,
                    upgrade_fut,
                } => {
                    inner.update_external_state(
                        &mut self.state_gen,
                        ApiInstanceState::Migrating,
                    );

                    drop(inner);

                    match self.migrate_as_source(
                        &controller,
                        migration_id,
                        upgrade_fut,
                    ) {
                        Ok(()) => {
                            info!(
                                self.log,
                                "Halting after successful migration out"
                            );

                            let mut inner =
                                controller.worker_state.inner.lock().unwrap();
                            inner.migrate_from_pending = false;
                            inner.halt_pending = true;
                            inner
                                .external_request_queue
                                .push_back(ExternalRequest::Stop);
                        }
                        Err(e) => {
                            info!(
                                self.log,
                                "Resuming after failed migration out ({})", e
                            );

                            let mut inner =
                                controller.worker_state.inner.lock().unwrap();
                            inner.migrate_from_pending = false;
                            inner.lifecycle_stage = LifecycleStage::Active;
                        }
                    }
                }
                ExternalRequest::Stop => {
                    (next_external, next_lifecycle) =
                        self.do_halt(inner, &controller);
                }
            }
        }

        info!(self.log, "State worker exiting");
    }

    fn handle_guest_event(
        &mut self,
        event: GuestEvent,
        inner: std::sync::MutexGuard<SharedVmStateInner>,
        controller: &VmController,
    ) -> (Option<ApiInstanceState>, Option<LifecycleStage>) {
        match event {
            GuestEvent::VcpuSuspendHalt(vcpu_id) => {
                info!(
                    self.log,
                    "Halting due to halt event on vCPU {}", vcpu_id
                );
                self.do_halt(inner, controller)
            }
            GuestEvent::VcpuSuspendReset(vcpu_id) => {
                info!(
                    self.log,
                    "Resetting due to reset event on vCPU {}", vcpu_id
                );
                (self.do_reboot(inner, controller), None)
            }
            GuestEvent::VcpuSuspendTripleFault(vcpu_id) => {
                info!(
                    self.log,
                    "Resetting due to triple fault on vCPU {}", vcpu_id
                );
                (self.do_reboot(inner, controller), None)
            }
            GuestEvent::ChipsetHalt => {
                info!(self.log, "Halting due to chipset-driven halt");
                self.do_halt(inner, controller)
            }
            GuestEvent::ChipsetReset => {
                info!(self.log, "Resetting due to chipset-driven reset");
                (self.do_reboot(inner, controller), None)
            }
        }
    }

    fn do_reboot(
        &mut self,
        mut inner: std::sync::MutexGuard<SharedVmStateInner>,
        controller: &VmController,
    ) -> Option<ApiInstanceState> {
        info!(self.log, "Resetting instance");

        // Reboots should only arrive after an instance has started.
        assert!(matches!(inner.lifecycle_stage, LifecycleStage::Active));

        inner.update_external_state(
            &mut self.state_gen,
            ApiInstanceState::Rebooting,
        );

        let next_external = Some(ApiInstanceState::Running);
        drop(inner);

        // Reboot is implemented as a pause -> reset -> resume
        // transition.
        //
        // First, pause the vCPUs and all entities so no
        // partially-completed work is present.
        self.vcpu_tasks.pause_all();
        self.pause_entities(controller);
        self.wait_for_entities_to_pause(controller);

        // Reset all the entities, then reset the VM's bhyve state,
        // then reset the vCPUs. The vCPU reset must come after the
        // bhyve reset.
        self.reset_entities(controller);
        controller.instance().lock().machine().reinitialize().unwrap();
        self.reset_vcpus(controller);

        // Resume entities so they're ready to do more work, then
        // resume vCPUs.
        self.resume_entities(controller);
        self.vcpu_tasks.resume_all();

        next_external
    }

    fn do_halt(
        &mut self,
        mut inner: std::sync::MutexGuard<SharedVmStateInner>,
        controller: &VmController,
    ) -> (Option<ApiInstanceState>, Option<LifecycleStage>) {
        info!(self.log, "Stopping instance");
        inner.update_external_state(
            &mut self.state_gen,
            ApiInstanceState::Stopping,
        );
        let next_external = Some(ApiInstanceState::Stopped);
        let next_lifecycle = Some(LifecycleStage::NoLongerActive);
        drop(inner);
        self.vcpu_tasks.exit_all();
        self.halt_entities(controller);
        (next_external, next_lifecycle)
    }

    fn migrate_as_target(
        &self,
        controller: &Arc<VmController>,
        migration_id: Uuid,
        upgrade_fut: hyper::upgrade::OnUpgrade,
    ) -> Result<(), MigrateError> {
        let log_for_task =
            self.log.new(slog::o!("component" => "migrate_target_task"));
        let ctrl_for_task = controller.clone();
        let (command_tx, mut command_rx) = tokio::sync::mpsc::channel(1);

        info!(self.log, "Launching migration target task");
        let mut task = self.runtime_hdl.spawn(async move {
            let conn = match upgrade_fut.await {
                Ok(upgraded) => upgraded,
                Err(e) => {
                    error!(log_for_task, "Connection upgrade failed: {}", e);
                    return Err(e.into());
                }
            };

            if let Err(e) = crate::migrate::destination::migrate(
                ctrl_for_task,
                command_tx,
                conn,
            )
            .await
            {
                error!(log_for_task, "Migration task failed: {}", e);
                return Err(e);
            }

            Ok(())
        });

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
                        let mut inner =
                            controller.worker_state.inner.lock().unwrap();
                        inner.migration_state =
                            Some((migration_id, ApiMigrationState::Error));
                    } else {
                        assert!(matches!(
                            controller
                                .worker_state
                                .inner
                                .lock()
                                .unwrap()
                                .migration_state,
                            Some((_, ApiMigrationState::Finish))
                        ));
                    }
                    return res;
                }
                MigrateTaskEvent::Command(
                    MigrateTargetCommand::UpdateState(state),
                ) => {
                    let mut inner =
                        controller.worker_state.inner.lock().unwrap();
                    inner.migration_state = Some((migration_id, state));
                }
            }
        }
    }

    fn migrate_as_source(
        &mut self,
        controller: &Arc<VmController>,
        migration_id: Uuid,
        upgrade_fut: hyper::upgrade::OnUpgrade,
    ) -> Result<(), MigrateError> {
        let log_for_task =
            self.log.new(slog::o!("component" => "migrate_source_task"));
        let ctrl_for_task = controller.clone();
        let (command_tx, mut command_rx) = tokio::sync::mpsc::channel(1);
        let (response_tx, response_rx) = tokio::sync::mpsc::channel(1);

        // The migration process uses async operations when communicating with
        // the migration target. Run that work on the async runtime.
        info!(self.log, "Launching migration source task");
        let mut task = self.runtime_hdl.spawn(async move {
            let conn = match upgrade_fut.await {
                Ok(upgraded) => upgraded,
                Err(e) => {
                    error!(log_for_task, "Connection upgrade failed: {}", e);
                    return Err(e.into());
                }
            };

            if let Err(e) = crate::migrate::source::migrate(
                ctrl_for_task,
                command_tx,
                response_rx,
                conn,
            )
            .await
            {
                error!(log_for_task, "Migration task failed: {}", e);
                return Err(e);
            }

            Ok(())
        });

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
                            self.resume_entities(controller);
                            self.vcpu_tasks.resume_all();
                            self.paused = false;
                            let mut inner =
                                controller.worker_state.inner.lock().unwrap();
                            inner.migration_state =
                                Some((migration_id, ApiMigrationState::Error));
                        }
                    } else {
                        assert!(matches!(
                            controller
                                .worker_state
                                .inner
                                .lock()
                                .unwrap()
                                .migration_state,
                            Some((_, ApiMigrationState::Finish))
                        ));
                    }
                    return res;
                }
                MigrateTaskEvent::Command(cmd) => match cmd {
                    MigrateSourceCommand::UpdateState(state) => {
                        let mut inner =
                            controller.worker_state.inner.lock().unwrap();
                        inner.migration_state = Some((migration_id, state));
                    }
                    MigrateSourceCommand::Pause => {
                        self.vcpu_tasks.pause_all();
                        self.pause_entities(controller);
                        self.wait_for_entities_to_pause(controller);
                        self.paused = true;
                        response_tx
                            .blocking_send(MigrateSourceResponse::Pause(Ok(())))
                            .unwrap();
                    }
                },
            }
        }
    }

    async fn next_migrate_task_event<T>(
        task: &mut tokio::task::JoinHandle<Result<(), MigrateError>>,
        command_rx: &mut tokio::sync::mpsc::Receiver<T>,
        log: &Logger,
    ) -> MigrateTaskEvent<T> {
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

    fn reset_vcpus(&self, controller: &VmController) {
        self.vcpu_tasks.new_generation();
        for vcpu in controller.instance().lock().machine().vcpus.iter() {
            info!(self.log, "Resetting vCPU {}", vcpu.id);
            vcpu.activate().unwrap();
            vcpu.reboot_state().unwrap();
            if vcpu.is_bsp() {
                info!(self.log, "Resetting BSP vCPU {}", vcpu.id);
                vcpu.set_run_state(propolis::bhyve_api::VRS_RUN, None).unwrap();
                vcpu.set_reg(
                    propolis::bhyve_api::vm_reg_name::VM_REG_GUEST_RIP,
                    0xfff0,
                )
                .unwrap();
            }
        }
    }

    fn start_entities(&self, controller: &VmController) -> anyhow::Result<()> {
        self.for_each_entity(controller, |ent, rec| {
            info!(self.log, "Sending startup complete to {}", rec.name());
            let res = ent.start();
            if let Err(e) = &res {
                error!(self.log, "Startup failed for {}: {:?}", rec.name(), e);
            }
            res
        })
    }

    fn pause_entities(&self, controller: &VmController) {
        self.for_each_entity(controller, |ent, rec| {
            info!(self.log, "Sending pause request to {}", rec.name());
            ent.pause();
            Ok(())
        })
        .unwrap();
    }

    fn reset_entities(&self, controller: &VmController) {
        self.for_each_entity(controller, |ent, rec| {
            info!(self.log, "Sending reset request to {}", rec.name());
            ent.reset();
            Ok(())
        })
        .unwrap();
    }

    fn resume_entities(&self, controller: &VmController) {
        self.for_each_entity(controller, |ent, rec| {
            info!(self.log, "Sending resume request to {}", rec.name());
            ent.resume();
            Ok(())
        })
        .unwrap();
    }

    fn halt_entities(&self, controller: &VmController) {
        self.for_each_entity(controller, |ent, rec| {
            info!(self.log, "Sending halt request to {}", rec.name());
            ent.halt();
            Ok(())
        })
        .unwrap();
    }

    fn for_each_entity<F>(
        &self,
        controller: &VmController,
        mut func: F,
    ) -> anyhow::Result<()>
    where
        F: FnMut(
            &Arc<dyn propolis::inventory::Entity>,
            &propolis::inventory::Record,
        ) -> anyhow::Result<()>,
    {
        controller.instance().lock().inventory().for_each_node(
            propolis::inventory::Order::Pre,
            |_eid, record| -> Result<(), anyhow::Error> {
                let ent = record.entity();
                func(ent, record)
            },
        )
    }

    fn wait_for_entities_to_pause(&self, controller: &VmController) {
        info!(self.log, "Waiting for all entities to pause");
        self.runtime_hdl.block_on(async {
            let mut devices = vec![];
            controller
                .instance()
                .lock()
                .inventory()
                .for_each_node(
                    propolis::inventory::Order::Post,
                    |_eid, record| {
                        info!(
                            self.log,
                            "Got paused future from entity {}",
                            record.name()
                        );
                        devices.push(Arc::clone(record.entity()));
                        Ok::<_, ()>(())
                    },
                )
                .unwrap();

            let pause_futures = devices.iter().map(|ent| ent.paused());
            futures::future::join_all(pause_futures).await;
        });
    }
}
