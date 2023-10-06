// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::time::Duration;

use phd_testcase::*;
use propolis_client::types::InstanceState;

#[phd_testcase]
fn boot_test(ctx: &TestContext) {
    let mut config = ctx.vm_config_builder("crucible_boot_test");
    super::add_default_boot_disk(ctx, &mut config)?;
    let mut vm = ctx.spawn_vm(&config, None)?;
    vm.launch()?;
    vm.wait_to_boot()?;
}

#[phd_testcase]
fn shutdown_persistence_test(ctx: &TestContext) {
    let mut config =
        ctx.vm_config_builder("crucible_shutdown_persistence_test");
    super::add_default_boot_disk(ctx, &mut config)?;
    let mut vm = ctx.spawn_vm(&config, None)?;
    if vm.guest_os_has_read_only_fs() {
        phd_skip!(
            "Can't run data persistence test on a guest with a read-only file
             system"
        );
    }

    let disk_handles = vm.cloned_disk_handles();
    let disk = disk_handles[0].as_crucible().unwrap();
    disk.set_generation(1);
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
    let mut vm = ctx.spawn_successor_vm(
        "crucible_shutdown_persistence_test_2",
        &vm,
        None,
    )?;

    vm.launch()?;
    vm.wait_to_boot()?;

    // The touched file from the previous VM should be present in the new one.
    let lsout = vm.run_shell_command("ls foo.bar 2> /dev/null")?;
    assert_eq!(lsout, "foo.bar");
}
