//! Tests verifying the server state machine.

use std::time::Duration;

use phd_testcase::*;
use propolis_client::{api::InstanceState, Error as ClientError};

#[phd_testcase]
fn instance_start_stop_test(ctx: &TestContext) {
    let mut vm = ctx.vm_factory.new_vm(
        "instance_ensure_running_test",
        ctx.vm_factory.default_vm_config(),
    )?;

    vm.launch()?;
    vm.wait_for_state(InstanceState::Running, Duration::from_secs(60))?;

    vm.stop()?;
    vm.wait_for_state(InstanceState::Destroyed, Duration::from_secs(60))?;
}

#[phd_testcase]
fn instance_stop_causes_destroy_test(ctx: &TestContext) {
    let mut vm = ctx.vm_factory.new_vm(
        "instance_stop_causes_destroy_test",
        ctx.vm_factory.default_vm_config(),
    )?;

    vm.launch()?;
    vm.stop()?;
    vm.wait_for_state(InstanceState::Destroyed, Duration::from_secs(60))?;

    assert!(matches!(
        vm.launch().unwrap_err().downcast_ref::<ClientError>().unwrap(),
        ClientError::Status(500)
    ));
    assert!(matches!(vm.stop().unwrap_err(), ClientError::Status(500)));
    assert!(matches!(vm.reset().unwrap_err(), ClientError::Status(500)));
}

#[phd_testcase]
fn instance_reset_returns_to_running_test(ctx: &TestContext) {
    let mut vm = ctx.vm_factory.new_vm(
        "instance_stop_returns_to_running_test",
        ctx.vm_factory.default_vm_config(),
    )?;

    vm.launch()?;
    vm.wait_for_state(InstanceState::Running, Duration::from_secs(60))?;
    vm.reset()?;
    vm.wait_for_state(InstanceState::Running, Duration::from_secs(60))?;
    vm.stop()?;
    vm.wait_for_state(InstanceState::Destroyed, Duration::from_secs(60))?;
}
