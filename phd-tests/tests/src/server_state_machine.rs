// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Tests verifying the server state machine.

use std::time::Duration;

use phd_framework::{
    disk::{BlockSize, DiskSource},
    test_vm::{DiskBackend, DiskInterface},
};
use phd_testcase::*;
use propolis_client::types::InstanceState;

#[phd_testcase]
async fn instance_start_stop_test(ctx: &Framework) {
    let mut vm = ctx.spawn_default_vm("instance_ensure_running_test").await?;

    vm.instance_ensure().await?;
    let instance = vm.get().await?.instance;
    assert_eq!(instance.state, InstanceState::Creating);

    vm.launch().await?;
    vm.wait_for_state(InstanceState::Running, Duration::from_secs(60)).await?;

    vm.stop().await?;
    vm.wait_for_state(InstanceState::Destroyed, Duration::from_secs(60))
        .await?;
}

#[phd_testcase]
async fn instance_stop_unstarted_test(ctx: &Framework) {
    let mut vm = ctx.spawn_default_vm("instance_stop_unstarted_test").await?;

    vm.instance_ensure().await?;
    let instance = vm.get().await?.instance;
    assert_eq!(instance.state, InstanceState::Creating);

    // At this point the VM is created and its resources are held as
    // appropriate. Stopping the VM will cause propolis-server to destroy the
    // VM, releasing those resources and getting the server ready for shutdown.
    vm.stop().await?;
    vm.wait_for_state(InstanceState::Destroyed, Duration::from_secs(60))
        .await?;
}

#[phd_testcase]
async fn instance_stop_causes_destroy_test(ctx: &Framework) {
    let mut vm =
        ctx.spawn_default_vm("instance_stop_causes_destroy_test").await?;

    vm.launch().await?;
    vm.stop().await?;
    vm.wait_for_state(InstanceState::Destroyed, Duration::from_secs(60))
        .await?;

    assert!(matches!(
        vm.run().await.unwrap_err().status().unwrap(),
        reqwest::StatusCode::FAILED_DEPENDENCY
    ));
    assert!(matches!(
        vm.stop().await.unwrap_err().status().unwrap(),
        reqwest::StatusCode::FAILED_DEPENDENCY
    ));
    assert!(matches!(
        vm.reset().await.unwrap_err().status().unwrap(),
        reqwest::StatusCode::FAILED_DEPENDENCY
    ));
}

#[phd_testcase]
async fn instance_reset_test(ctx: &Framework) {
    let mut vm =
        ctx.spawn_default_vm("instance_reset_returns_to_running_test").await?;

    assert!(vm.reset().await.is_err());
    vm.launch().await?;
    vm.wait_for_state(InstanceState::Running, Duration::from_secs(60)).await?;

    // Because "Rebooting" is not a steady state, the Propolis state worker may
    // transition the instance from Running to Rebooting to Running before the
    // test gets a chance to monitor its state any further. Thus, it's not safe
    // to test that the Rebooting state was observed here.
    vm.reset().await?;
    vm.wait_for_state(InstanceState::Running, Duration::from_secs(60)).await?;

    // Queue multiple reset attempts. These should all succeed, even though
    // Propolis will only allow one reboot to be enqueued at a time. Once again,
    // the specific number of reboots that will be queued depends on factors
    // outside of the test's control.
    for _ in 0..10 {
        vm.reset().await?;
    }

    vm.wait_for_state(InstanceState::Running, Duration::from_secs(60)).await?;
    vm.stop().await?;
    vm.wait_for_state(InstanceState::Destroyed, Duration::from_secs(60))
        .await?;
    assert!(vm.reset().await.is_err());
}

#[phd_testcase]
async fn instance_reset_requires_running_test(ctx: &Framework) {
    let mut vm =
        ctx.spawn_default_vm("instance_reset_requires_running_test").await?;

    assert!(vm.reset().await.is_err());
    vm.launch().await?;
    vm.wait_for_state(InstanceState::Running, Duration::from_secs(60)).await?;
}

#[phd_testcase]
async fn stop_while_blocked_on_start_test(ctx: &Framework) {
    // This test uses a Crucible disk backend to cause VM startup to block.
    if !ctx.crucible_enabled() {
        phd_skip!("test requires Crucible support");
    }

    let mut config = ctx.vm_config_builder("stop_while_blocked_on_start_test");

    // Create a VM that blocks while starting by attaching a Crucible data disk
    // to it and enabling the black hole address in its volume construction
    // request. The invalid address will keep Crucible from activating and so
    // will block the VM from fully starting.
    const DATA_DISK_NAME: &str = "vcr-replacement-target";
    config.data_disk(
        DATA_DISK_NAME,
        DiskSource::Blank(1024 * 1024 * 1024),
        DiskInterface::Nvme,
        DiskBackend::Crucible {
            min_disk_size_gib: 1,
            block_size: BlockSize::Bytes512,
        },
        5,
    );

    let spec = config.vm_spec(ctx).await?;
    let disk_hdl =
        spec.get_disk_by_device_name(DATA_DISK_NAME).cloned().unwrap();
    let disk = disk_hdl.as_crucible().unwrap();
    disk.enable_vcr_black_hole();

    // Launch the VM and wait for it to advertise that its components are
    // starting.
    let mut vm = ctx.spawn_vm_with_spec(spec, None).await?;
    vm.launch().await?;
    vm.wait_for_state(InstanceState::Starting, Duration::from_secs(15))
        .await
        .unwrap();

    // Send a stop request. This should enqueue successfully, and the VM should
    // shut down even though activation is blocked.
    vm.stop().await?;
    vm.wait_for_state(InstanceState::Destroyed, Duration::from_secs(60))
        .await?;
}
