//! Tests verifying the server state machine.

use std::time::Duration;

use phd_testcase::*;
use propolis_client::api::InstanceState;

#[phd_testcase]
fn instance_start_stop_test(ctx: &TestContext) {
    let vm = ctx.vm_factory.new_vm(
        "instance_ensure_running_test",
        ctx.vm_factory.default_vm_config(),
    )?;

    let instance = vm.get()?.instance;
    assert_eq!(instance.state, InstanceState::Creating);

    vm.run()?;
    vm.wait_for_state(InstanceState::Running, Duration::from_secs(60))?;

    vm.stop()?;
    vm.wait_for_state(InstanceState::Destroyed, Duration::from_secs(60))?;
}

#[phd_testcase]
fn instance_stop_causes_destroy_test(ctx: &TestContext) {
    let vm = ctx.vm_factory.new_vm(
        "instance_stop_causes_destroy_test",
        ctx.vm_factory.default_vm_config(),
    )?;

    vm.run()?;
    vm.stop()?;
    vm.wait_for_state(InstanceState::Destroyed, Duration::from_secs(60))?;

    assert!(vm.run().is_err());
    assert!(vm.stop().is_err());
    assert!(vm.reset().is_err());
}

#[phd_testcase]
fn instance_reset_returns_to_running_test(ctx: &TestContext) {
    let vm = ctx.vm_factory.new_vm(
        "instance_stop_returns_to_running_test",
        ctx.vm_factory.default_vm_config(),
    )?;

    vm.run()?;
    vm.wait_for_state(InstanceState::Running, Duration::from_secs(60))?;
    vm.reset()?;
    vm.wait_for_state(InstanceState::Running, Duration::from_secs(60))?;
    vm.stop()?;
    vm.wait_for_state(InstanceState::Destroyed, Duration::from_secs(60))?;
}
