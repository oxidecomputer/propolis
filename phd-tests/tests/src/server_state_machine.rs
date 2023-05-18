//! Tests verifying the server state machine.

use std::time::Duration;

use phd_testcase::*;
use propolis_client::types::InstanceState;

#[phd_testcase]
fn instance_start_stop_test(ctx: &TestContext) {
    let mut vm = ctx
        .vm_factory
        .new_vm("instance_ensure_running_test", ctx.default_vm_config())?;

    vm.instance_ensure()?;
    let instance = vm.get()?.instance;
    assert_eq!(instance.state, InstanceState::Creating);

    vm.launch()?;
    vm.wait_for_state(InstanceState::Running, Duration::from_secs(60))?;

    vm.stop()?;
    vm.wait_for_state(InstanceState::Destroyed, Duration::from_secs(60))?;
}

#[phd_testcase]
fn instance_stop_causes_destroy_test(ctx: &TestContext) {
    let mut vm = ctx
        .vm_factory
        .new_vm("instance_stop_causes_destroy_test", ctx.default_vm_config())?;

    vm.launch()?;
    vm.stop()?;
    vm.wait_for_state(InstanceState::Destroyed, Duration::from_secs(60))?;

    assert!(matches!(
        vm.run().unwrap_err().status().unwrap(),
        http::status::StatusCode::NOT_FOUND
    ));
    assert!(matches!(
        vm.stop().unwrap_err().status().unwrap(),
        http::status::StatusCode::NOT_FOUND
    ));
    assert!(matches!(
        vm.reset().unwrap_err().status().unwrap(),
        http::status::StatusCode::NOT_FOUND
    ));
}

#[phd_testcase]
fn instance_reset_test(ctx: &TestContext) {
    let mut vm = ctx.vm_factory.new_vm(
        "instance_reset_returns_to_running_test",
        ctx.default_vm_config(),
    )?;

    assert!(vm.reset().is_err());
    vm.launch()?;
    vm.wait_for_state(InstanceState::Running, Duration::from_secs(60))?;

    // Because "Rebooting" is not a steady state, the Propolis state worker may
    // transition the instance from Running to Rebooting to Running before the
    // test gets a chance to monitor its state any further. Thus, it's not safe
    // to test that the Rebooting state was observed here.
    vm.reset()?;
    vm.wait_for_state(InstanceState::Running, Duration::from_secs(60))?;

    // Queue multiple reset attempts. These should all succeed, even though
    // Propolis will only allow one reboot to be enqueued at a time. Once again,
    // the specific number of reboots that will be queued depends on factors
    // outside of the test's control.
    for _ in 0..10 {
        vm.reset()?;
    }

    vm.wait_for_state(InstanceState::Running, Duration::from_secs(60))?;
    vm.stop()?;
    vm.wait_for_state(InstanceState::Destroyed, Duration::from_secs(60))?;
    assert!(vm.reset().is_err());
}

#[phd_testcase]
fn instance_reset_requires_running_test(ctx: &TestContext) {
    let mut vm = ctx.vm_factory.new_vm(
        "instance_reset_requires_running_test",
        ctx.default_vm_config(),
    )?;

    assert!(vm.reset().is_err());
    vm.launch()?;
    vm.wait_for_state(InstanceState::Running, Duration::from_secs(60))?;
}
