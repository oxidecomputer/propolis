use std::time::Duration;

use phd_testcase::{phd_framework::test_vm::vm_config::DiskInterface, *};
use propolis_client::handmade::api::InstanceState;

#[phd_testcase]
fn boot_test(ctx: &TestContext) {
    let disk = super::create_default_boot_disk(ctx, 10)?;
    let config = ctx.deviceless_vm_config().set_boot_disk(
        disk.clone(),
        4,
        DiskInterface::Nvme,
    );

    let mut vm = ctx.vm_factory.new_vm("crucible_boot_test", config)?;
    vm.launch()?;
    vm.wait_to_boot()?;
}

#[phd_testcase]
fn shutdown_persistence_test(ctx: &TestContext) {
    let disk = super::create_default_boot_disk(ctx, 10)?;
    disk.set_generation(1);
    let config = ctx.deviceless_vm_config().set_boot_disk(
        disk.clone(),
        4,
        DiskInterface::Nvme,
    );

    let mut vm =
        ctx.vm_factory.new_vm("crucible_shutdown_persistence_test", config)?;
    if vm.guest_os_has_read_only_fs() {
        phd_skip!(
            "Can't run data persistence test on a guest with a read-only file
             system"
        );
    }

    vm.launch()?;
    vm.wait_to_boot()?;

    // Verify that the test file doesn't exist yet, then touch it, flush it, and
    // shut down the VM.
    let lsout = vm.run_shell_command("ls foo.bar 2> /dev/null")?;
    assert_eq!(lsout, "");
    vm.run_shell_command("touch ./foo.bar")?;
    vm.run_shell_command("sync ./foo.bar")?;
    vm.stop()?;
    vm.wait_for_state(InstanceState::Destroyed, Duration::from_secs(60))?;

    // Increment the disk's generation before attaching it to a new VM.
    disk.set_generation(2);
    let config = ctx.deviceless_vm_config().set_boot_disk(
        disk.clone(),
        4,
        DiskInterface::Nvme,
    );

    // The touched file from the previous VM should be present in the new one.
    let mut vm = ctx
        .vm_factory
        .new_vm("crucible_shutdown_persistence_test_2", config)?;

    vm.launch()?;
    vm.wait_to_boot()?;
    let lsout = vm.run_shell_command("ls foo.bar 2> /dev/null")?;
    assert_eq!(lsout, "foo.bar");
}
