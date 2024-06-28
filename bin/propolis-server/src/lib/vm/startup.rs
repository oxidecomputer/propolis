// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Functionality used to create a new VM and possibly live migrate into it.

use std::sync::Arc;

use propolis_api_types::{
    instance_spec::VersionedInstanceSpec, InstanceProperties,
    InstanceSpecEnsureRequest, MigrationState,
};
use slog::{error, info};
use uuid::Uuid;

use crate::{
    initializer::{
        build_instance, MachineInitializer, MachineInitializerState,
    },
    migrate::{MigrateError, MigrateRole},
};

use super::{
    migrate_commands::{
        next_migrate_task_event, MigrateTargetCommand, MigrateTargetResponse,
        MigrateTaskEvent,
    },
    objects::{InputVmObjects, VmObjects},
    state_driver::InputQueue,
    state_publisher::{
        ExternalStateUpdate, MigrationStateUpdate, StatePublisher,
    },
};

/// The context needed to finish a live migration into a VM after its initial
/// Sync phase has concluded and produced a set of VM objects (into which the
/// migration will import the source VM's state).
pub(super) struct MigrateAsTargetContext {
    /// The objects into which to import state from the source.
    vm_objects: Arc<VmObjects>,

    /// The logger associated with this migration.
    log: slog::Logger,

    /// The migration's ID.
    migration_id: Uuid,

    /// A handle to the task that's driving the migration.
    migrate_task: tokio::task::JoinHandle<Result<(), MigrateError>>,

    /// Receives commands from the migration task.
    command_rx: tokio::sync::mpsc::Receiver<MigrateTargetCommand>,

    /// Sends command responses to the migration task.
    response_tx: tokio::sync::mpsc::Sender<MigrateTargetResponse>,
}

/// The output of a call to [`build_vm`].
pub(super) struct BuildVmOutput {
    /// A reference to the VM objects created by the request to build a new VM.
    pub vm_objects: Arc<VmObjects>,

    /// If the VM is initializing via migration in, the context needed to
    /// complete that migration.
    pub migration_in: Option<MigrateAsTargetContext>,
}

/// Builds a new set of VM objects from the supplied ensure `request`.
///
/// If the request asks to create a new VM without migrating, this routine
/// simply sets up the new VM's objects and returns them.
///
/// Callers who ask to initialize a VM via live migration expect their API calls
/// to succeed as soon as there's an initialized VM and a running migration
/// task, even if the migration hasn't completed yet. To facilitate this, when
/// initializing via live migration, this routine executes only enough of the
/// live migration protocol to create VM objects, then immediately returns those
/// objects and a context the caller can use to finish the migration task. This
/// allows the caller to complete any external ensure calls it has pending
/// before completing migration and allowing the state driver to process state
/// change requests.
pub(super) async fn build_vm(
    log: &slog::Logger,
    parent: &Arc<super::Vm>,
    request: &InstanceSpecEnsureRequest,
    options: &super::EnsureOptions,
    input_queue: &Arc<InputQueue>,
    state_publisher: &mut StatePublisher,
) -> anyhow::Result<BuildVmOutput, (anyhow::Error, Option<Arc<VmObjects>>)> {
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

        return Ok(BuildVmOutput { vm_objects, migration_in: None });
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
    let migrate_task = tokio::spawn(async move {
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

    // In the initial phases of live migration, the migration protocol decides
    // whether the source and destination VMs have compatible configurations. If
    // they do, the migration task asks this routine to initialize a VM on its
    // behalf. Execute this part of the protocol now in order to create a set of
    // VM objects to return.
    //
    // TODO(#706): Future versions of the protocol can extend this further,
    // specifying an instance spec and/or an initial set of device payloads that
    // the task should use to initialize its VM objects.
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

    // The VM's objects are initialized. Return them to the caller along with a
    // continuation context that it can use to complete migration.
    let migration_in = MigrateAsTargetContext {
        vm_objects: vm_objects.clone(),
        log: log.clone(),
        migration_id: migrate_ctx.migration_id,
        migrate_task,
        command_rx,
        response_tx,
    };

    Ok(BuildVmOutput { vm_objects, migration_in: Some(migration_in) })
}

/// Initializes a set of Propolis components from the supplied instance spec.
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
        event_queue.clone() as Arc<dyn super::guest_event::VcpuEventHandler>,
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

impl MigrateAsTargetContext {
    /// Runs a partially-completed inbound live migration to completion.
    pub(super) async fn run(
        mut self,
        state_publisher: &mut StatePublisher,
    ) -> Result<(), MigrateError> {
        // The migration task imports device state by operating directly on the
        // newly-created VM objects. Before sending them to the task, make sure
        // the objects are ready to have state imported into them. Specifically,
        // ensure that the VM's vCPUs are activated so they can enter the guest
        // after migration and pause the kernel VM to allow it to import device
        // state consistently.
        //
        // Drop the lock after this operation so that the migration task can
        // acquire it to enumerate devices and import state into them.
        {
            let guard = self.vm_objects.read().await;
            guard.reset_vcpus();
            guard.pause_kernel_vm();
        }

        self.response_tx
            .send(MigrateTargetResponse::VmObjectsInitialized(Ok(self
                .vm_objects
                .clone())))
            .await
            .expect("migration task shouldn't exit while awaiting driver");

        loop {
            let action = next_migrate_task_event(
                &mut self.migrate_task,
                &mut self.command_rx,
                &self.log,
            )
            .await;

            match action {
                MigrateTaskEvent::TaskExited(res) => match res {
                    Ok(()) => {
                        return Ok(());
                    }
                    Err(e) => {
                        error!(self.log, "target migration task failed";
                           "error" => %e);

                        self.vm_objects.write().await.resume_kernel_vm();
                        return Err(e);
                    }
                },
                MigrateTaskEvent::Command(
                    MigrateTargetCommand::UpdateState(state),
                ) => {
                    state_publisher.update(ExternalStateUpdate::Migration(
                        MigrationStateUpdate {
                            state,
                            id: self.migration_id,
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
}
