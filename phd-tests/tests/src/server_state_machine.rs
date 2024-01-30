// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Tests verifying the server state machine.

use std::time::Duration;

use phd_testcase::*;
use propolis_client::types::InstanceState;

#[phd_testcase]
async fn instance_start_stop_test(ctx: &Framework) {
    let mut vm = ctx.spawn_default_vm("instance_ensure_running_test")?;

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
async fn instance_stop_causes_destroy_test(ctx: &Framework) {
    let mut vm = ctx.spawn_default_vm("instance_stop_causes_destroy_test")?;

    vm.launch().await?;
    vm.stop().await?;
    vm.wait_for_state(InstanceState::Destroyed, Duration::from_secs(60))
        .await?;

    assert!(matches!(
        vm.run().await.unwrap_err().status().unwrap(),
        http::status::StatusCode::NOT_FOUND
    ));
    assert!(matches!(
        vm.stop().await.unwrap_err().status().unwrap(),
        http::status::StatusCode::NOT_FOUND
    ));
    assert!(matches!(
        vm.reset().await.unwrap_err().status().unwrap(),
        http::status::StatusCode::NOT_FOUND
    ));
}

#[phd_testcase]
async fn instance_reset_test(ctx: &Framework) {
    let mut vm =
        ctx.spawn_default_vm("instance_reset_returns_to_running_test")?;

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
        ctx.spawn_default_vm("instance_reset_requires_running_test")?;

    assert!(vm.reset().await.is_err());
    vm.launch().await?;
    vm.wait_for_state(InstanceState::Running, Duration::from_secs(60)).await?;
}
