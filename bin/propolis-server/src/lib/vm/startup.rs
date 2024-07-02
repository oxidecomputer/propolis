// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Functionality used to create a new VM and possibly live migrate into it.

use std::sync::Arc;

use propolis_api_types::{
    instance_spec::VersionedInstanceSpec, InstanceProperties,
    InstanceSpecEnsureRequest,
};
use slog::info;

use crate::{
    initializer::{
        build_instance, MachineInitializer, MachineInitializerState,
    },
    migrate::destination::DestinationProtocol,
};

use super::{
    objects::{InputVmObjects, VmObjects},
    state_driver::InputQueue,
    state_publisher::StatePublisher,
};

type BuildVmError = (anyhow::Error, Option<Arc<VmObjects>>);

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
) -> Result<Arc<VmObjects>, BuildVmError> {
    let make_objects = async {
        let input_objects = initialize_vm_objects_from_spec(
            log,
            input_queue,
            &request.properties,
            &request.instance_spec,
            options,
        )
        .await
        .map_err(|e| (e, None))?;

        Ok(Arc::new(VmObjects::new(log.clone(), parent.clone(), input_objects)))
    };

    // If the caller didn't ask to initialize by live migration in, immediately
    // create the VM objects and return them.
    let Some(migrate_request) = &request.migrate else {
        return make_objects.await;
    };

    let mut runner = crate::migrate::destination::initiate(
        log,
        migrate_request,
        options.local_server_addr,
        state_publisher,
    )
    .await
    .map_err(|e| (e.into(), None))?;

    runner.run_pre_vm_init().await.map_err(|e| (e.into(), None))?;
    let vm_objects = make_objects.await?;

    // The migration task imports device state by operating directly on the
    // newly-created VM objects. Before sending them to the task, make sure
    // the objects are ready to have state imported into them. Specifically,
    // ensure that the VM's vCPUs are activated so they can enter the guest
    // after migration and pause the kernel VM to allow it to import device
    // state consistently.
    //
    // Drop the lock after this operation so that the migration task can
    // acquire it to enumerate devices and import state into them.
    let result = {
        let mut guard = vm_objects.lock_exclusive().await;
        guard.reset_vcpus();
        guard.pause_kernel_vm();
        runner.run_post_vm_init(&mut guard).await
    };

    match result {
        Ok(()) => Ok(vm_objects),
        Err(e) => {
            vm_objects.lock_exclusive().await.resume_kernel_vm();
            Err((e.into(), Some(vm_objects)))
        }
    }
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
